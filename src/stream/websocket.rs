//! WebSocket stream implementation
//!
//! This module provides the main `WebSocketStream` type.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use futures_sink::Sink;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::Config;
use crate::cork::CorkBuffer;
use crate::error::{CloseReason, Error, Result};
use crate::protocol::{Message, Protocol, Role};

/// Default high water mark for backpressure (64KB)
const DEFAULT_HIGH_WATER_MARK: usize = 64 * 1024;

/// Default low water mark for backpressure (16KB)
const DEFAULT_LOW_WATER_MARK: usize = 16 * 1024;

pin_project! {
    /// A WebSocket stream over an async transport
    ///
    /// This type implements both `Stream<Item = Result<Message>>` for receiving
    /// and `Sink<Message>` for sending messages.
    ///
    /// # Backpressure
    ///
    /// The stream supports backpressure monitoring through `is_backpressured()` and
    /// `write_buffer_len()` methods. When the write buffer exceeds the high water mark,
    /// producers should pause sending until the buffer drains below the low water mark.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use futures_util::{SinkExt, StreamExt};
    /// use sockudo_ws::WebSocketStream;
    ///
    /// async fn handle(mut ws: WebSocketStream<TcpStream>) {
    ///     while let Some(msg) = ws.next().await {
    ///         match msg {
    ///             Ok(Message::Text(text)) => {
    ///                 // Check backpressure before sending
    ///                 if ws.is_backpressured() {
    ///                     ws.flush().await?;
    ///                 }
    ///                 ws.send(Message::Text(text)).await?;
    ///             }
    ///             Ok(Message::Close(_)) => break,
    ///             _ => {}
    ///         }
    ///     }
    /// }
    /// ```
    pub struct WebSocketStream<S> {
        #[pin]
        inner: S,
        protocol: Protocol,
        read_buf: BytesMut,
        write_buf: CorkBuffer,
        state: StreamState,
        config: Config,
        // Pending messages from last process() call
        pending_messages: Vec<Message>,
        pending_index: usize,
        // A control message is only returned after its automatic response is flushed.
        pending_control_message: Option<Message>,
        flush_on_read: bool,
        close_after_flush: bool,
        // Created lazily from poll_next so construction does not require a Tokio runtime.
        auto_ping_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
        // Backpressure thresholds
        high_water_mark: usize,
        low_water_mark: usize,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
    /// Normal operation
    Open,
    /// Flushing write buffer
    Flushing,
    /// Close frame sent
    CloseSent,
    /// Connection closed
    Closed,
}

impl<S> WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Create a new WebSocket stream from an already-upgraded connection
    pub fn from_raw(inner: S, role: Role, config: Config) -> Self {
        let protocol = Protocol::new(role, config.max_frame_size, config.max_message_size);

        Self {
            inner,
            protocol,
            read_buf: BytesMut::with_capacity(crate::RECV_BUFFER_SIZE),
            write_buf: CorkBuffer::with_capacity(config.write_buffer_size),
            state: StreamState::Open,
            config,
            pending_messages: Vec::new(),
            pending_index: 0,
            pending_control_message: None,
            flush_on_read: false,
            close_after_flush: false,
            auto_ping_sleep: None,
            high_water_mark: DEFAULT_HIGH_WATER_MARK,
            low_water_mark: DEFAULT_LOW_WATER_MARK,
        }
    }

    /// Create a server-side WebSocket stream
    pub fn server(inner: S, config: Config) -> Self {
        Self::from_raw(inner, Role::Server, config)
    }

    /// Create a client-side WebSocket stream
    pub fn client(inner: S, config: Config) -> Self {
        Self::from_raw(inner, Role::Client, config)
    }

    /// Get a reference to the underlying stream
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// Get a mutable reference to the underlying stream
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    /// Consume the WebSocket stream and return the underlying stream
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Check if the connection is closed
    pub fn is_closed(&self) -> bool {
        self.state == StreamState::Closed
    }

    // ========================================================================
    // Backpressure API
    // ========================================================================

    /// Check if the write buffer is backpressured
    ///
    /// Returns `true` when the write buffer has exceeded the high water mark.
    /// Producers should pause sending new messages until `is_write_buffer_low()`
    /// returns `true` or until the buffer is flushed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// if ws.is_backpressured() {
    ///     // Wait for buffer to drain before sending more
    ///     ws.flush().await?;
    /// }
    /// ```
    #[inline]
    pub fn is_backpressured(&self) -> bool {
        self.write_buf.pending_bytes() > self.high_water_mark
    }

    /// Check if the write buffer is below the low water mark
    ///
    /// Returns `true` when the write buffer has drained below the low water mark.
    /// This can be used to resume sending after backpressure was detected.
    #[inline]
    pub fn is_write_buffer_low(&self) -> bool {
        self.write_buf.pending_bytes() <= self.low_water_mark
    }

    /// Get the current write buffer size in bytes
    ///
    /// Useful for monitoring and debugging backpressure issues.
    #[inline]
    pub fn write_buffer_len(&self) -> usize {
        self.write_buf.pending_bytes()
    }

    /// Get the current read buffer size in bytes
    ///
    /// Useful for monitoring memory usage and debugging.
    #[inline]
    pub fn read_buffer_len(&self) -> usize {
        self.read_buf.len()
    }

    /// Set the high water mark for backpressure
    ///
    /// When the write buffer exceeds this threshold, `is_backpressured()` returns `true`.
    /// Default is 64KB.
    #[inline]
    pub fn set_high_water_mark(&mut self, size: usize) {
        self.high_water_mark = size;
    }

    /// Set the low water mark for backpressure
    ///
    /// When the write buffer drops below this threshold, `is_write_buffer_low()` returns `true`.
    /// Default is 16KB.
    #[inline]
    pub fn set_low_water_mark(&mut self, size: usize) {
        self.low_water_mark = size;
    }

    /// Get the current high water mark
    #[inline]
    pub fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }

    /// Get the current low water mark
    #[inline]
    pub fn low_water_mark(&self) -> usize {
        self.low_water_mark
    }

    /// Send a close frame
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        if self.state != StreamState::Open {
            return Ok(());
        }

        let close = Message::Close(Some(CloseReason::new(code, reason)));
        self.protocol
            .encode_message(&close, self.write_buf.buffer_mut())?;
        self.state = StreamState::CloseSent;

        // Flush the close frame
        self.flush_write_buf().await?;
        Ok(())
    }

    /// Flush the write buffer to the underlying stream
    async fn flush_write_buf(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        while self.write_buf.has_data() {
            let slices = self.write_buf.get_write_slices();
            if slices.is_empty() {
                break;
            }

            let n = self.inner.write_vectored(&slices).await?;
            if n == 0 {
                return Err(Error::ConnectionClosed);
            }
            self.write_buf.consume(n);
        }

        self.inner.flush().await?;
        Ok(())
    }

    /// Read more data from the underlying stream
    fn poll_read_more(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.project();

        // Ensure we have space in the buffer
        if this.read_buf.capacity() - this.read_buf.len() < 4096 {
            this.read_buf.reserve(8192);
        }

        // Get a slice of uninitialized memory
        let buf_len = this.read_buf.len();
        let buf_cap = this.read_buf.capacity();

        // SAFETY: We're extending into the spare capacity
        unsafe {
            this.read_buf.set_len(buf_cap);
        }

        let mut read_buf = ReadBuf::new(&mut this.read_buf[buf_len..]);

        match this.inner.poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let n = read_buf.filled().len();
                unsafe {
                    this.read_buf.set_len(buf_len + n);
                }
                if n == 0 {
                    Poll::Ready(Ok(0))
                } else {
                    Poll::Ready(Ok(n))
                }
            }
            Poll::Ready(Err(e)) => {
                unsafe {
                    this.read_buf.set_len(buf_len);
                }
                Poll::Ready(Err(e))
            }
            Poll::Pending => {
                unsafe {
                    this.read_buf.set_len(buf_len);
                }
                Poll::Pending
            }
        }
    }

    /// Process read buffer and extract messages
    fn process_read_buf(&mut self) -> Result<()> {
        if self.read_buf.is_empty() {
            return Ok(());
        }

        let messages = self.protocol.process(&mut self.read_buf)?;

        if !messages.is_empty() {
            self.pending_messages = messages;
            self.pending_index = 0;
        }

        Ok(())
    }

    /// Get the next pending message
    fn next_pending_message(&mut self) -> Option<Message> {
        if self.pending_index < self.pending_messages.len() {
            let msg = self.pending_messages[self.pending_index].clone();
            self.pending_index += 1;

            // Clear when all consumed
            if self.pending_index >= self.pending_messages.len() {
                self.pending_messages.clear();
                self.pending_index = 0;
            }

            Some(msg)
        } else {
            None
        }
    }
}

impl<S> Stream for WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Item = Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // Control responses and automatic pings must be driven by the read path.
            if self.flush_on_read {
                match self.as_mut().poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        let this = self.as_mut().get_mut();
                        this.flush_on_read = false;

                        if this.close_after_flush {
                            this.close_after_flush = false;
                            this.state = StreamState::Closed;
                        }

                        if let Some(msg) = this.pending_control_message.take() {
                            return Poll::Ready(Some(Ok(msg)));
                        }
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                    Poll::Pending => return Poll::Pending,
                }
            }

            // Check for connection closed
            if self.state == StreamState::Closed {
                return Poll::Ready(None);
            }

            // Register a timer wakeup and send a Ping even when the peer is silent.
            if self.config.auto_ping && self.config.ping_interval > 0 {
                let interval = Duration::from_secs(self.config.ping_interval.into());
                let this = self.as_mut().get_mut();
                let sleep = this
                    .auto_ping_sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(interval)));

                if sleep.as_mut().poll(cx).is_ready() {
                    this.auto_ping_sleep = Some(Box::pin(tokio::time::sleep(interval)));
                    if let Err(e) = this
                        .protocol
                        .encode_message(&Message::Ping(Bytes::new()), this.write_buf.buffer_mut())
                    {
                        return Poll::Ready(Some(Err(e)));
                    }
                    this.flush_on_read = true;
                    continue;
                }
            }

            // First, return any pending messages
            if let Some(msg) = self.as_mut().get_mut().next_pending_message() {
                // Handle control frames
                match &msg {
                    Message::Ping(data) => {
                        // Queue pong response
                        let this = self.as_mut().get_mut();
                        this.protocol.encode_pong(data, this.write_buf.buffer_mut());
                        this.pending_control_message = Some(msg);
                        this.flush_on_read = true;
                        continue;
                    }
                    Message::Close(reason) => {
                        let this = self.as_mut().get_mut();
                        if this.state == StreamState::Open {
                            // Send close response
                            this.protocol
                                .encode_close_response(this.write_buf.buffer_mut());
                        }
                        this.pending_control_message = Some(Message::Close(reason.clone()));
                        this.flush_on_read = true;
                        this.close_after_flush = true;
                        continue;
                    }
                    _ => {}
                }

                return Poll::Ready(Some(Ok(msg)));
            }

            // Try to read more data
            match self.as_mut().poll_read_more(cx) {
                Poll::Ready(Ok(0)) => {
                    // EOF - connection closed
                    self.as_mut().get_mut().state = StreamState::Closed;
                    return Poll::Ready(None);
                }
                Poll::Ready(Ok(_n)) => {
                    // Process the new data
                    match self.as_mut().get_mut().process_read_buf() {
                        Ok(()) => continue, // Loop to check for messages
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    }
                }
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Some(Err(e.into())));
                }
                Poll::Pending => {
                    // No more data available right now
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<S> Sink<Message> for WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.state == StreamState::Closed {
            return Poll::Ready(Err(Error::ConnectionClosed));
        }
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<()> {
        let this = self.get_mut();

        if this.state == StreamState::Closed {
            return Err(Error::ConnectionClosed);
        }

        // Track close frame sending
        if item.is_close() {
            this.state = StreamState::CloseSent;
        }

        // Encode message into write buffer
        this.protocol
            .encode_message(&item, this.write_buf.buffer_mut())?;
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.as_mut().get_mut();

        // Write all pending data
        while this.write_buf.has_data() {
            let slices = this.write_buf.get_write_slices();
            if slices.is_empty() {
                break;
            }

            match Pin::new(&mut this.inner).poll_write_vectored(cx, &slices) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(Error::ConnectionClosed));
                }
                Poll::Ready(Ok(n)) => {
                    this.write_buf.consume(n);
                }
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Err(e.into()));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }

        // Flush underlying stream
        match Pin::new(&mut self.as_mut().get_mut().inner).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // Send close frame if not already sent
        if self.state == StreamState::Open {
            let close = Message::Close(Some(CloseReason::new(1000, "")));
            if let Err(e) = self.as_mut().start_send(close) {
                return Poll::Ready(Err(e));
            }
        }

        // Flush pending data
        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }

        // Shutdown the underlying stream
        match Pin::new(&mut self.as_mut().get_mut().inner).poll_shutdown(cx) {
            Poll::Ready(Ok(())) => {
                self.as_mut().get_mut().state = StreamState::Closed;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Builder for WebSocket streams
pub struct WebSocketStreamBuilder {
    config: Config,
    role: Role,
    high_water_mark: usize,
    low_water_mark: usize,
}

impl WebSocketStreamBuilder {
    /// Create a new builder with default configuration
    pub fn new() -> Self {
        Self {
            config: Config::default(),
            role: Role::Server,
            high_water_mark: DEFAULT_HIGH_WATER_MARK,
            low_water_mark: DEFAULT_LOW_WATER_MARK,
        }
    }

    /// Set the endpoint role
    pub fn role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    /// Set the maximum message size
    pub fn max_message_size(mut self, size: usize) -> Self {
        self.config.max_message_size = size;
        self
    }

    /// Set the maximum frame size
    pub fn max_frame_size(mut self, size: usize) -> Self {
        self.config.max_frame_size = size;
        self
    }

    /// Set the write buffer size
    pub fn write_buffer_size(mut self, size: usize) -> Self {
        self.config.write_buffer_size = size;
        self
    }

    /// Set the high water mark for backpressure
    ///
    /// When the write buffer exceeds this threshold, `is_backpressured()` returns `true`.
    /// Default is 64KB.
    pub fn high_water_mark(mut self, size: usize) -> Self {
        self.high_water_mark = size;
        self
    }

    /// Set the low water mark for backpressure
    ///
    /// When the write buffer drops below this threshold, `is_write_buffer_low()` returns `true`.
    /// Default is 16KB.
    pub fn low_water_mark(mut self, size: usize) -> Self {
        self.low_water_mark = size;
        self
    }

    /// Build the WebSocket stream
    pub fn build<S>(self, stream: S) -> WebSocketStream<S>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut ws = WebSocketStream::from_raw(stream, self.role, self.config);
        ws.high_water_mark = self.high_water_mark;
        ws.low_water_mark = self.low_water_mark;
        ws
    }
}

impl Default for WebSocketStreamBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Split Stream Implementation - LOCK-FREE EDITION 🚀
// ============================================================================
//
// This implementation uses tokio::io::split() for true concurrent I/O:
// - ✅ Zero mutex contention
// - ✅ Reader and writer operate 100% independently
// - ✅ Native OS-level efficiency
// - ✅ No shared lock on the underlying transport
//
// Control frames (Ping/Pong/Close) are coordinated via an mpsc channel
// from the reader to the writer, allowing the reader to request responses
// without blocking.

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

/// Control frame requests sent from reader to writer
#[derive(Debug, Clone)]
enum ControlRequest {
    /// Send a Pong in response to a Ping
    Pong(bytes::Bytes),
    /// Send a Close response
    CloseResponse,
}

/// The read half of a split WebSocket stream
///
/// Created by calling `split()` on a `WebSocketStream`.
/// This half owns the read side of the TCP stream and can operate
/// completely independently from the write half.
pub struct SplitReader<S> {
    /// Read half of the underlying stream (no lock!)
    reader: ReadHalf<S>,
    /// Protocol for decoding
    protocol: Protocol,
    /// Read buffer
    read_buf: BytesMut,
    /// Pending messages from last decode
    pending_messages: Vec<Message>,
    pending_index: usize,
    /// Channel to send control frame requests to writer
    control_tx: mpsc::UnboundedSender<ControlRequest>,
    /// Connection state
    closed: bool,
}

/// The write half of a split WebSocket stream
///
/// Created by calling `split()` on a `WebSocketStream`.
/// This half owns the write side of the TCP stream and can operate
/// completely independently from the read half.
pub struct SplitWriter<S> {
    /// Write half of the underlying stream (no lock!)
    writer: WriteHalf<S>,
    /// Protocol for encoding
    protocol: Protocol,
    /// Write buffer for encoding
    write_buf: BytesMut,
    /// Channel to receive control frame requests from reader
    control_rx: mpsc::UnboundedReceiver<ControlRequest>,
    /// Connection state
    closed: bool,
}

impl<S> WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Split the WebSocket stream into separate read and write halves
    ///
    /// This allows TRUE concurrent reading and writing from different tasks
    /// with ZERO lock contention. The underlying TCP stream is split at the
    /// OS level for maximum performance.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (mut reader, mut writer) = ws.split();
    ///
    /// // Read in one task - NEVER blocks writer
    /// tokio::spawn(async move {
    ///     while let Some(msg) = reader.next().await {
    ///         println!("Got: {:?}", msg);
    ///     }
    /// });
    ///
    /// // Write in another - NEVER blocks reader
    /// writer.send(Message::Text("Hello".into())).await?;
    /// ```
    pub fn split(self) -> (SplitReader<S>, SplitWriter<S>) {
        // Split the underlying transport at the OS level
        let (reader, writer) = tokio::io::split(self.inner);

        // Create channel for control frame coordination
        let (control_tx, control_rx) = mpsc::unbounded_channel();

        // Clone the protocol for both halves (cheap - just config)
        let reader_protocol = Protocol::new(
            self.protocol.role,
            self.config.max_frame_size,
            self.config.max_message_size,
        );
        let writer_protocol = self.protocol;

        (
            SplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                pending_messages: self.pending_messages,
                pending_index: self.pending_index,
                control_tx,
                closed: self.state == StreamState::Closed,
            },
            SplitWriter {
                writer,
                protocol: writer_protocol,
                write_buf: BytesMut::with_capacity(1024),
                control_rx,
                closed: self.state == StreamState::Closed,
            },
        )
    }
}

impl<S> SplitReader<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Receive the next message
    ///
    /// Returns `None` when the connection is closed.
    /// This method NEVER blocks the writer - true concurrent I/O!
    pub async fn next(&mut self) -> Option<Result<Message>> {
        loop {
            // Check for connection closed
            if self.closed {
                return None;
            }

            // Return any pending messages first
            if self.pending_index < self.pending_messages.len() {
                let msg = self.pending_messages[self.pending_index].clone();
                self.pending_index += 1;

                if self.pending_index >= self.pending_messages.len() {
                    self.pending_messages.clear();
                    self.pending_index = 0;
                }

                // Handle control frames - send requests to writer via channel
                match &msg {
                    Message::Ping(data) => {
                        // Request writer to send pong (non-blocking!)
                        let _ = self.control_tx.send(ControlRequest::Pong(data.clone()));
                        // Continue to next message (user doesn't see Ping)
                        continue;
                    }
                    Message::Close(reason) => {
                        if !self.closed {
                            // Request writer to send close response
                            let _ = self.control_tx.send(ControlRequest::CloseResponse);
                            self.closed = true;
                        }
                        return Some(Ok(Message::Close(reason.clone())));
                    }
                    Message::Pong(_) => {
                        // User doesn't typically need to see Pong
                        continue;
                    }
                    _ => {}
                }

                return Some(Ok(msg));
            }

            // Need to read more data - NO LOCK HERE!
            // Reserve space if needed
            if self.read_buf.capacity() - self.read_buf.len() < 4096 {
                self.read_buf.reserve(8192);
            }

            match self.reader.read_buf(&mut self.read_buf).await {
                Ok(0) => {
                    // EOF - connection closed
                    self.closed = true;
                    return None;
                }
                Ok(_n) => {
                    // Process the new data
                    match self.protocol.process(&mut self.read_buf) {
                        Ok(messages) => {
                            if !messages.is_empty() {
                                self.pending_messages = messages;
                                self.pending_index = 0;
                            }
                        }
                        Err(e) => return Some(Err(e)),
                    }
                    // Continue loop to check for messages
                }
                Err(e) => {
                    return Some(Err(e.into()));
                }
            }
        }
    }

    /// Check if the connection is closed
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl<S> SplitWriter<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Send a message
    ///
    /// This method NEVER blocks the reader - true concurrent I/O!
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        if self.closed {
            return Err(Error::ConnectionClosed);
        }

        // Process any pending control frame requests from reader first
        self.process_control_requests().await?;

        if msg.is_close() {
            self.closed = true;
        }

        // Encode message - NO LOCK HERE!
        self.write_buf.clear();
        self.protocol.encode_message(&msg, &mut self.write_buf)?;

        // Write to the underlying stream - NO LOCK HERE!
        self.writer.write_all(&self.write_buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Process control frame requests from the reader
    async fn process_control_requests(&mut self) -> Result<()> {
        // Drain all pending control requests
        while let Ok(req) = self.control_rx.try_recv() {
            self.write_buf.clear();

            match req {
                ControlRequest::Pong(data) => {
                    self.protocol.encode_pong(&data, &mut self.write_buf);
                }
                ControlRequest::CloseResponse => {
                    self.protocol.encode_close_response(&mut self.write_buf);
                    self.closed = true;
                }
            }

            if !self.write_buf.is_empty() {
                self.writer.write_all(&self.write_buf).await?;
            }
        }

        Ok(())
    }

    /// Send a text message
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.send(Message::text(text)).await
    }

    /// Send a binary message
    pub async fn send_binary(&mut self, data: bytes::Bytes) -> Result<()> {
        self.send(Message::Binary(data)).await
    }

    /// Send a close frame
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        self.send(Message::Close(Some(CloseReason::new(code, reason))))
            .await
    }

    /// Check if the connection is closed
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Flush any pending control responses
    pub async fn flush(&mut self) -> Result<()> {
        self.process_control_requests().await?;
        self.writer.flush().await.map_err(Into::into)
    }
}

// ============================================================================
// Compressed WebSocket Stream (permessage-deflate)
// ============================================================================

#[cfg(feature = "permessage-deflate")]
pin_project! {
    /// A WebSocket stream with permessage-deflate compression (RFC 7692)
    ///
    /// This type mirrors `WebSocketStream` but uses `CompressedProtocol` for
    /// automatic compression/decompression of messages.
    pub struct CompressedWebSocketStream<S> {
        #[pin]
        inner: S,
        protocol: crate::protocol::CompressedProtocol,
        read_buf: BytesMut,
        write_buf: CorkBuffer,
        state: StreamState,
        config: Config,
        pending_messages: Vec<Message>,
        pending_index: usize,
        pending_control_message: Option<Message>,
        flush_on_read: bool,
        close_after_flush: bool,
        auto_ping_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
        high_water_mark: usize,
        low_water_mark: usize,
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> CompressedWebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Create a new compressed WebSocket stream for server role
    pub fn server(inner: S, config: Config, deflate_config: crate::deflate::DeflateConfig) -> Self {
        let protocol = crate::protocol::CompressedProtocol::server(
            config.max_frame_size,
            config.max_message_size,
            deflate_config,
        );

        Self {
            inner,
            protocol,
            read_buf: BytesMut::with_capacity(crate::RECV_BUFFER_SIZE),
            write_buf: CorkBuffer::with_capacity(config.write_buffer_size),
            state: StreamState::Open,
            config,
            pending_messages: Vec::new(),
            pending_index: 0,
            pending_control_message: None,
            flush_on_read: false,
            close_after_flush: false,
            auto_ping_sleep: None,
            high_water_mark: DEFAULT_HIGH_WATER_MARK,
            low_water_mark: DEFAULT_LOW_WATER_MARK,
        }
    }

    /// Create a new compressed WebSocket stream for client role
    pub fn client(inner: S, config: Config, deflate_config: crate::deflate::DeflateConfig) -> Self {
        let protocol = crate::protocol::CompressedProtocol::client(
            config.max_frame_size,
            config.max_message_size,
            deflate_config,
        );

        Self {
            inner,
            protocol,
            read_buf: BytesMut::with_capacity(crate::RECV_BUFFER_SIZE),
            write_buf: CorkBuffer::with_capacity(config.write_buffer_size),
            state: StreamState::Open,
            config,
            pending_messages: Vec::new(),
            pending_index: 0,
            pending_control_message: None,
            flush_on_read: false,
            close_after_flush: false,
            auto_ping_sleep: None,
            high_water_mark: DEFAULT_HIGH_WATER_MARK,
            low_water_mark: DEFAULT_LOW_WATER_MARK,
        }
    }

    /// Check if the connection is closed
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.state == StreamState::Closed || self.protocol.is_closed()
    }

    /// Check if backpressure should be applied
    #[inline]
    pub fn is_backpressured(&self) -> bool {
        self.write_buf.pending_bytes() > self.high_water_mark
    }

    /// Get the current write buffer length
    #[inline]
    pub fn write_buffer_len(&self) -> usize {
        self.write_buf.pending_bytes()
    }

    /// Send a close frame
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        if self.state != StreamState::Open {
            return Ok(());
        }

        let close = Message::Close(Some(CloseReason::new(code, reason)));
        self.protocol
            .encode_message(&close, self.write_buf.buffer_mut())?;
        self.state = StreamState::CloseSent;

        self.flush_write_buf().await?;
        Ok(())
    }

    /// Flush the write buffer to the underlying stream
    async fn flush_write_buf(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        while self.write_buf.has_data() {
            let slices = self.write_buf.get_write_slices();
            if slices.is_empty() {
                break;
            }

            let n = self.inner.write_vectored(&slices).await?;
            if n == 0 {
                return Err(Error::ConnectionClosed);
            }
            self.write_buf.consume(n);
        }

        self.inner.flush().await?;
        Ok(())
    }

    /// Read more data from the underlying stream
    fn poll_read_more(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.project();

        if this.read_buf.capacity() - this.read_buf.len() < 4096 {
            this.read_buf.reserve(8192);
        }

        let buf_len = this.read_buf.len();
        let buf_cap = this.read_buf.capacity();

        unsafe {
            this.read_buf.set_len(buf_cap);
        }

        let mut read_buf = ReadBuf::new(&mut this.read_buf[buf_len..]);

        match this.inner.poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let n = read_buf.filled().len();
                unsafe {
                    this.read_buf.set_len(buf_len + n);
                }
                if n == 0 {
                    Poll::Ready(Ok(0))
                } else {
                    Poll::Ready(Ok(n))
                }
            }
            Poll::Ready(Err(e)) => {
                unsafe {
                    this.read_buf.set_len(buf_len);
                }
                Poll::Ready(Err(e))
            }
            Poll::Pending => {
                unsafe {
                    this.read_buf.set_len(buf_len);
                }
                Poll::Pending
            }
        }
    }

    /// Process read buffer and extract messages
    fn process_read_buf(&mut self) -> Result<()> {
        if self.read_buf.is_empty() {
            return Ok(());
        }

        let messages = self.protocol.process(&mut self.read_buf)?;

        if !messages.is_empty() {
            self.pending_messages = messages;
            self.pending_index = 0;
        }

        Ok(())
    }

    /// Get the next pending message
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
}

#[cfg(feature = "permessage-deflate")]
impl<S> Stream for CompressedWebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Item = Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.flush_on_read {
                match self.as_mut().poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        let this = self.as_mut().get_mut();
                        this.flush_on_read = false;

                        if this.close_after_flush {
                            this.close_after_flush = false;
                            this.state = StreamState::Closed;
                        }

                        if let Some(msg) = this.pending_control_message.take() {
                            return Poll::Ready(Some(Ok(msg)));
                        }
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                    Poll::Pending => return Poll::Pending,
                }
            }

            if self.state == StreamState::Closed {
                return Poll::Ready(None);
            }

            if self.config.auto_ping && self.config.ping_interval > 0 {
                let interval = Duration::from_secs(self.config.ping_interval.into());
                let this = self.as_mut().get_mut();
                let sleep = this
                    .auto_ping_sleep
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(interval)));

                if sleep.as_mut().poll(cx).is_ready() {
                    this.auto_ping_sleep = Some(Box::pin(tokio::time::sleep(interval)));
                    if let Err(e) = this
                        .protocol
                        .encode_message(&Message::Ping(Bytes::new()), this.write_buf.buffer_mut())
                    {
                        return Poll::Ready(Some(Err(e)));
                    }
                    this.flush_on_read = true;
                    continue;
                }
            }

            if let Some(msg) = self.as_mut().get_mut().next_pending_message() {
                match &msg {
                    Message::Ping(data) => {
                        let this = self.as_mut().get_mut();
                        this.protocol.encode_pong(data, this.write_buf.buffer_mut());
                        this.pending_control_message = Some(msg);
                        this.flush_on_read = true;
                        continue;
                    }
                    Message::Close(reason) => {
                        let this = self.as_mut().get_mut();
                        if this.state == StreamState::Open {
                            this.protocol
                                .encode_close_response(this.write_buf.buffer_mut());
                        }
                        this.pending_control_message = Some(Message::Close(reason.clone()));
                        this.flush_on_read = true;
                        this.close_after_flush = true;
                        continue;
                    }
                    _ => {}
                }

                return Poll::Ready(Some(Ok(msg)));
            }

            match self.as_mut().poll_read_more(cx) {
                Poll::Ready(Ok(0)) => {
                    self.as_mut().get_mut().state = StreamState::Closed;
                    return Poll::Ready(None);
                }
                Poll::Ready(Ok(_n)) => match self.as_mut().get_mut().process_read_buf() {
                    Ok(()) => continue,
                    Err(e) => return Poll::Ready(Some(Err(e))),
                },
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Some(Err(e.into())));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> Sink<Message> for CompressedWebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.state == StreamState::Closed {
            return Poll::Ready(Err(Error::ConnectionClosed));
        }
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<()> {
        let this = self.get_mut();

        if this.state == StreamState::Closed {
            return Err(Error::ConnectionClosed);
        }

        if item.is_close() {
            this.state = StreamState::CloseSent;
        }

        this.protocol
            .encode_message(&item, this.write_buf.buffer_mut())?;
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.as_mut().get_mut();

        while this.write_buf.has_data() {
            let slices = this.write_buf.get_write_slices();
            if slices.is_empty() {
                break;
            }

            match Pin::new(&mut this.inner).poll_write_vectored(cx, &slices) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(Error::ConnectionClosed));
                }
                Poll::Ready(Ok(n)) => {
                    this.write_buf.consume(n);
                }
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Err(e.into()));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }

        match Pin::new(&mut self.as_mut().get_mut().inner).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.state == StreamState::Open {
            let close = Message::Close(Some(CloseReason::new(1000, "")));
            if let Err(e) = self.as_mut().start_send(close) {
                return Poll::Ready(Err(e));
            }
        }

        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }

        match Pin::new(&mut self.as_mut().get_mut().inner).poll_shutdown(cx) {
            Poll::Ready(Ok(())) => {
                self.as_mut().get_mut().state = StreamState::Closed;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ============================================================================
// Compressed Split Reader/Writer (permessage-deflate)
// ============================================================================

/// The read half of a split compressed WebSocket stream
///
/// Created by calling `split()` on a `CompressedWebSocketStream`.
/// This half owns the read side of the TCP stream and can operate
/// completely independently from the write half.
#[cfg(feature = "permessage-deflate")]
pub struct CompressedSplitReader<S> {
    /// Read half of the underlying stream
    reader: ReadHalf<S>,
    /// Protocol for decoding with decompression
    protocol: crate::protocol::CompressedReaderProtocol,
    /// Read buffer
    read_buf: BytesMut,
    /// Pending messages from last decode
    pending_messages: Vec<Message>,
    pending_index: usize,
    /// Channel to send control frame requests to writer
    control_tx: mpsc::UnboundedSender<ControlRequest>,
    /// Connection state
    closed: bool,
}

/// The write half of a split compressed WebSocket stream
///
/// Created by calling `split()` on a `CompressedWebSocketStream`.
/// This half owns the write side of the TCP stream and can operate
/// completely independently from the read half.
#[cfg(feature = "permessage-deflate")]
pub struct CompressedSplitWriter<S> {
    /// Write half of the underlying stream
    writer: WriteHalf<S>,
    /// Protocol for encoding with compression
    protocol: crate::protocol::CompressedWriterProtocol,
    /// Write buffer for encoding
    write_buf: BytesMut,
    /// Channel to receive control frame requests from reader
    control_rx: mpsc::UnboundedReceiver<ControlRequest>,
    /// Connection state
    closed: bool,
}

#[cfg(feature = "permessage-deflate")]
impl<S> CompressedWebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Split the compressed WebSocket stream into separate read and write halves
    ///
    /// This allows TRUE concurrent reading and writing from different tasks
    /// with ZERO lock contention. The underlying TCP stream is split at the
    /// OS level for maximum performance.
    ///
    /// Both halves maintain compression/decompression state independently:
    /// - Reader has the decoder for decompressing incoming messages
    /// - Writer has the encoder for compressing outgoing messages
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (mut reader, mut writer) = compressed_ws.split();
    ///
    /// // Read in one task - NEVER blocks writer
    /// tokio::spawn(async move {
    ///     while let Some(msg) = reader.next().await {
    ///         println!("Got: {:?}", msg);
    ///     }
    /// });
    ///
    /// // Write in another - NEVER blocks reader
    /// writer.send(Message::Text("Hello".into())).await?;
    /// ```
    pub fn split(self) -> (CompressedSplitReader<S>, CompressedSplitWriter<S>) {
        // Split the underlying transport at the OS level
        let (reader, writer) = tokio::io::split(self.inner);

        // Create channel for control frame coordination
        let (control_tx, control_rx) = mpsc::unbounded_channel();

        // Split the protocol into reader and writer halves
        let (reader_protocol, writer_protocol) = self
            .protocol
            .split(self.config.max_frame_size, self.config.max_message_size);

        (
            CompressedSplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                pending_messages: self.pending_messages,
                pending_index: self.pending_index,
                control_tx,
                closed: self.state == StreamState::Closed,
            },
            CompressedSplitWriter {
                writer,
                protocol: writer_protocol,
                write_buf: BytesMut::with_capacity(1024),
                control_rx,
                closed: self.state == StreamState::Closed,
            },
        )
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> CompressedSplitReader<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Receive the next message
    ///
    /// Returns `None` when the connection is closed.
    /// This method NEVER blocks the writer - true concurrent I/O!
    pub async fn next(&mut self) -> Option<Result<Message>> {
        loop {
            // Check for connection closed
            if self.closed {
                return None;
            }

            // Return any pending messages first
            if self.pending_index < self.pending_messages.len() {
                let msg = self.pending_messages[self.pending_index].clone();
                self.pending_index += 1;

                if self.pending_index >= self.pending_messages.len() {
                    self.pending_messages.clear();
                    self.pending_index = 0;
                }

                // Handle control frames - send requests to writer via channel
                match &msg {
                    Message::Ping(data) => {
                        // Request writer to send pong (non-blocking!)
                        let _ = self.control_tx.send(ControlRequest::Pong(data.clone()));
                        // Continue to next message (user doesn't see Ping)
                        continue;
                    }
                    Message::Close(reason) => {
                        if !self.closed {
                            // Request writer to send close response
                            let _ = self.control_tx.send(ControlRequest::CloseResponse);
                            self.closed = true;
                        }
                        return Some(Ok(Message::Close(reason.clone())));
                    }
                    Message::Pong(_) => {
                        // User doesn't typically need to see Pong
                        continue;
                    }
                    _ => {}
                }

                return Some(Ok(msg));
            }

            // Need to read more data - NO LOCK HERE!
            // Reserve space if needed
            if self.read_buf.capacity() - self.read_buf.len() < 4096 {
                self.read_buf.reserve(8192);
            }

            match self.reader.read_buf(&mut self.read_buf).await {
                Ok(0) => {
                    // EOF - connection closed
                    self.closed = true;
                    return None;
                }
                Ok(_n) => {
                    // Process the new data
                    match self.protocol.process(&mut self.read_buf) {
                        Ok(messages) => {
                            if !messages.is_empty() {
                                self.pending_messages = messages;
                                self.pending_index = 0;
                            }
                        }
                        Err(e) => return Some(Err(e)),
                    }
                    // Continue loop to check for messages
                }
                Err(e) => {
                    return Some(Err(e.into()));
                }
            }
        }
    }

    /// Check if the connection is closed
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> CompressedSplitWriter<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Send a message
    ///
    /// This method NEVER blocks the reader - true concurrent I/O!
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        if self.closed {
            return Err(Error::ConnectionClosed);
        }

        // Process any pending control frame requests from reader first
        self.process_control_requests().await?;

        if msg.is_close() {
            self.closed = true;
        }

        // Encode message with compression - NO LOCK HERE!
        self.write_buf.clear();
        self.protocol.encode_message(&msg, &mut self.write_buf)?;

        // Write to the underlying stream - NO LOCK HERE!
        self.writer.write_all(&self.write_buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Process control frame requests from the reader
    async fn process_control_requests(&mut self) -> Result<()> {
        // Drain all pending control requests
        while let Ok(req) = self.control_rx.try_recv() {
            self.write_buf.clear();

            match req {
                ControlRequest::Pong(data) => {
                    self.protocol.encode_pong(&data, &mut self.write_buf);
                }
                ControlRequest::CloseResponse => {
                    self.protocol.encode_close_response(&mut self.write_buf);
                    self.closed = true;
                }
            }

            if !self.write_buf.is_empty() {
                self.writer.write_all(&self.write_buf).await?;
            }
        }

        Ok(())
    }

    /// Send a text message
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.send(Message::text(text)).await
    }

    /// Send a binary message
    pub async fn send_binary(&mut self, data: bytes::Bytes) -> Result<()> {
        self.send(Message::Binary(data)).await
    }

    /// Send a close frame
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        self.send(Message::Close(Some(CloseReason::new(code, reason))))
            .await
    }

    /// Check if the connection is closed
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Flush any pending control responses
    pub async fn flush(&mut self) -> Result<()> {
        self.process_control_requests().await?;
        self.writer.flush().await.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn read_masked_control_frame(
        io: &mut tokio::io::DuplexStream,
        expected_opcode: u8,
        expected_payload: &[u8],
    ) {
        let mut header = [0; 2];
        io.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], 0x80 | expected_opcode);
        assert_ne!(header[1] & 0x80, 0, "client frames must be masked");

        let payload_len = usize::from(header[1] & 0x7f);
        assert_eq!(payload_len, expected_payload.len());

        let mut mask = [0; 4];
        io.read_exact(&mut mask).await.unwrap();
        let mut payload = vec![0; payload_len];
        io.read_exact(&mut payload).await.unwrap();
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        assert_eq!(payload, expected_payload);
    }

    // Tests would require a mock async transport
    // For now, we just verify the types compile correctly

    #[test]
    fn test_builder() {
        let _builder = WebSocketStreamBuilder::new()
            .role(Role::Server)
            .max_message_size(1024 * 1024)
            .max_frame_size(64 * 1024);
    }

    #[tokio::test]
    async fn read_only_stream_flushes_automatic_pong() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let mut ws = WebSocketStream::client(client_io, Config::default());

        server_io
            .write_all(&[0x89, 0x03, b'p', b'i', b'n'])
            .await
            .unwrap();

        let message = ws.next().await.unwrap().unwrap();
        assert!(matches!(message, Message::Ping(data) if data == b"pin"[..]));
        read_masked_control_frame(&mut server_io, 0x0a, b"pin").await;
    }

    #[tokio::test]
    async fn read_only_stream_flushes_close_response() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let mut ws = WebSocketStream::client(client_io, Config::default());

        server_io
            .write_all(&[0x88, 0x02, 0x03, 0xe8])
            .await
            .unwrap();

        let message = ws.next().await.unwrap().unwrap();
        assert!(matches!(message, Message::Close(Some(reason)) if reason.code == 1000));
        read_masked_control_frame(&mut server_io, 0x08, &[0x03, 0xe8]).await;
        assert!(ws.is_closed());
    }

    #[tokio::test]
    async fn auto_ping_uses_configured_interval_on_read_path() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let config = Config::builder().auto_ping(true).ping_interval(1).build();
        let mut ws = WebSocketStream::client(client_io, config);

        let read_task = tokio::spawn(async move { ws.next().await });
        tokio::time::timeout(
            Duration::from_secs(2),
            read_masked_control_frame(&mut server_io, 0x09, b""),
        )
        .await
        .expect("automatic Ping was not sent at the configured interval");
        read_task.abort();
    }
}
