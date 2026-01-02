//! WebSocket stream implementation
//!
//! This module provides the main `WebSocketStream` type.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::BytesMut;
use futures_core::Stream;
use futures_sink::Sink;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex;

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
            // Check for connection closed
            if self.state == StreamState::Closed {
                return Poll::Ready(None);
            }

            // First, return any pending messages
            if let Some(msg) = self.as_mut().get_mut().next_pending_message() {
                // Handle control frames
                match &msg {
                    Message::Ping(data) => {
                        // Queue pong response
                        let this = self.as_mut().get_mut();
                        this.protocol.encode_pong(data, this.write_buf.buffer_mut());
                    }
                    Message::Close(reason) => {
                        let this = self.as_mut().get_mut();
                        if this.state == StreamState::Open {
                            // Send close response
                            this.protocol
                                .encode_close_response(this.write_buf.buffer_mut());
                            this.state = StreamState::Closed;
                        }
                        return Poll::Ready(Some(Ok(Message::Close(reason.clone()))));
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
// Split Stream Implementation
// ============================================================================

/// Shared state between split read and write halves
struct SplitState<S> {
    inner: S,
    protocol: Protocol,
    read_buf: BytesMut,
    write_buf: CorkBuffer,
    state: StreamState,
    pending_messages: Vec<Message>,
    pending_index: usize,
}

/// The read half of a split WebSocket stream
///
/// Created by calling `split()` on a `WebSocketStream`.
pub struct SplitReader<S> {
    shared: Arc<Mutex<SplitState<S>>>,
}

/// The write half of a split WebSocket stream
///
/// Created by calling `split()` on a `WebSocketStream`.
pub struct SplitWriter<S> {
    shared: Arc<Mutex<SplitState<S>>>,
}

impl<S> WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Split the WebSocket stream into separate read and write halves
    ///
    /// This allows concurrent reading and writing from different tasks.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (mut reader, mut writer) = ws.split();
    ///
    /// // Read in one task
    /// tokio::spawn(async move {
    ///     while let Some(msg) = reader.next().await {
    ///         println!("Got: {:?}", msg);
    ///     }
    /// });
    ///
    /// // Write in another
    /// writer.send(Message::Text("Hello".into())).await?;
    /// ```
    pub fn split(self) -> (SplitReader<S>, SplitWriter<S>) {
        let shared = Arc::new(Mutex::new(SplitState {
            inner: self.inner,
            protocol: self.protocol,
            read_buf: self.read_buf,
            write_buf: self.write_buf,
            state: self.state,
            pending_messages: self.pending_messages,
            pending_index: self.pending_index,
        }));

        (
            SplitReader {
                shared: shared.clone(),
            },
            SplitWriter { shared },
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
    pub async fn next(&mut self) -> Option<Result<Message>> {
        use tokio::io::AsyncReadExt;

        loop {
            let mut state = self.shared.lock().await;

            // Check for connection closed
            if state.state == StreamState::Closed {
                return None;
            }

            // Return any pending messages first
            if state.pending_index < state.pending_messages.len() {
                let msg = state.pending_messages[state.pending_index].clone();
                state.pending_index += 1;

                if state.pending_index >= state.pending_messages.len() {
                    state.pending_messages.clear();
                    state.pending_index = 0;
                }

                // Handle control frames using destructuring to satisfy borrow checker
                match &msg {
                    Message::Ping(data) => {
                        let data = data.clone();
                        let SplitState {
                            protocol,
                            write_buf,
                            ..
                        } = &mut *state;
                        protocol.encode_pong(&data, write_buf.buffer_mut());
                    }
                    Message::Close(reason) => {
                        if state.state == StreamState::Open {
                            let SplitState {
                                protocol,
                                write_buf,
                                ..
                            } = &mut *state;
                            protocol.encode_close_response(write_buf.buffer_mut());
                            state.state = StreamState::Closed;
                        }
                        return Some(Ok(Message::Close(reason.clone())));
                    }
                    _ => {}
                }

                return Some(Ok(msg));
            }

            // Need to read more data
            // Use a temp buffer to avoid borrow issues
            let mut temp_buf = [0u8; 8192];
            let read_result = state.inner.read(&mut temp_buf).await;

            match read_result {
                Ok(0) => {
                    state.state = StreamState::Closed;
                    return None;
                }
                Ok(n) => {
                    state.read_buf.extend_from_slice(&temp_buf[..n]);

                    // Process the buffer using destructuring
                    let SplitState {
                        protocol,
                        read_buf,
                        pending_messages,
                        pending_index,
                        ..
                    } = &mut *state;
                    match protocol.process(read_buf) {
                        Ok(messages) => {
                            if !messages.is_empty() {
                                *pending_messages = messages;
                                *pending_index = 0;
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
}

impl<S> SplitWriter<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Send a message
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let mut state = self.shared.lock().await;

        if state.state == StreamState::Closed {
            return Err(Error::ConnectionClosed);
        }

        if msg.is_close() {
            state.state = StreamState::CloseSent;
        }

        // Encode message into a temporary buffer first
        let mut encode_buf = BytesMut::with_capacity(1024);
        state.protocol.encode_message(&msg, &mut encode_buf)?;

        // Write to the underlying stream
        state.inner.write_all(&encode_buf).await?;
        state.inner.flush().await?;
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
    pub async fn is_closed(&self) -> bool {
        let state = self.shared.lock().await;
        state.state == StreamState::Closed
    }
}

/// Reunite split reader and writer back into a WebSocketStream
///
/// Returns `Err` if the reader and writer are from different streams.
pub fn reunite<S>(
    reader: SplitReader<S>,
    writer: SplitWriter<S>,
) -> std::result::Result<WebSocketStream<S>, ReuniteError<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if !Arc::ptr_eq(&reader.shared, &writer.shared) {
        return Err(ReuniteError { reader, writer });
    }

    // Drop one reference
    drop(reader);

    // Extract the state
    let state = Arc::try_unwrap(writer.shared)
        .map_err(|arc| {
            // This shouldn't happen since we dropped reader
            let reader = SplitReader {
                shared: arc.clone(),
            };
            let writer = SplitWriter { shared: arc };
            ReuniteError { reader, writer }
        })?
        .into_inner();

    Ok(WebSocketStream {
        inner: state.inner,
        protocol: state.protocol,
        read_buf: state.read_buf,
        write_buf: state.write_buf,
        state: state.state,
        config: Config::default(),
        pending_messages: state.pending_messages,
        pending_index: state.pending_index,
        high_water_mark: DEFAULT_HIGH_WATER_MARK,
        low_water_mark: DEFAULT_LOW_WATER_MARK,
    })
}

/// Error returned when trying to reunite halves from different streams
pub struct ReuniteError<S> {
    /// The reader half
    pub reader: SplitReader<S>,
    /// The writer half
    pub writer: SplitWriter<S>,
}

impl<S> std::fmt::Debug for ReuniteError<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReuniteError")
            .field("reader", &"SplitReader { .. }")
            .field("writer", &"SplitWriter { .. }")
            .finish()
    }
}

impl<S> std::fmt::Display for ReuniteError<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tried to reunite halves from different WebSocketStreams")
    }
}

impl<S> std::error::Error for ReuniteError<S> {}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests would require a mock async transport
    // For now, we just verify the types compile correctly

    #[test]
    fn test_builder() {
        let _builder = WebSocketStreamBuilder::new()
            .role(Role::Server)
            .max_message_size(1024 * 1024)
            .max_frame_size(64 * 1024);
    }
}
