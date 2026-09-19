//! WebSocket stream implementation
//!
//! This module provides the main `WebSocketStream` type.

use std::io;
use std::io::IoSlice;
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
use crate::frame::{OpCode, encode_frame_header};
use crate::heartbeat::{Deadline, Heartbeat, bounded_close_reason};
use crate::protocol::{Message, Protocol, Role};

/// Default high water mark for backpressure (64KB)
const DEFAULT_HIGH_WATER_MARK: usize = 64 * 1024;

/// Default low water mark for backpressure (16KB)
const DEFAULT_LOW_WATER_MARK: usize = 16 * 1024;

/// Maximum number of IoSlices handed to one vectored write (stack allocated)
const MAX_WRITE_SLICES: usize = 16;

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
        // Leftover handshake bytes must be processed once before the first read.
        has_unprocessed_read_data: bool,
        write_buf: CorkBuffer,
        state: StreamState,
        config: Config,
        // Pending messages from last process() call
        pending_messages: Vec<Message>,
        // Deadline (ms since clock_epoch) the heartbeat timer is currently armed for
        heartbeat_armed_ms: u64,
        // A control message is only returned after its automatic response is flushed.
        pending_control_message: Option<Message>,
        pending_terminal_error: Option<Error>,
        flush_on_read: bool,
        close_after_flush: bool,
        ping_flush_pending: bool,
        clock_epoch: tokio::time::Instant,
        heartbeat: Heartbeat,
        heartbeat_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
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
        Self::from_raw_with_leftover(inner, role, config, None)
    }

    /// Create a WebSocket stream with bytes already read after the HTTP handshake.
    pub fn from_raw_with_leftover(
        inner: S,
        role: Role,
        config: Config,
        leftover: Option<Bytes>,
    ) -> Self {
        let protocol = Protocol::new(role, config.max_frame_size, config.max_message_size);
        let mut read_buf = BytesMut::with_capacity(crate::RECV_BUFFER_SIZE);
        if let Some(leftover) = leftover {
            read_buf.extend_from_slice(&leftover);
        }
        let has_unprocessed_read_data = !read_buf.is_empty();
        let clock_epoch = tokio::time::Instant::now();
        let heartbeat = Heartbeat::new(&config, 0);

        Self {
            inner,
            protocol,
            read_buf,
            has_unprocessed_read_data,
            write_buf: CorkBuffer::with_capacity(config.write_buffer_size),
            state: StreamState::Open,
            config,
            pending_messages: Vec::new(),
            heartbeat_armed_ms: 0,
            pending_control_message: None,
            pending_terminal_error: None,
            flush_on_read: false,
            close_after_flush: false,
            ping_flush_pending: false,
            clock_epoch,
            heartbeat,
            heartbeat_sleep: None,
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
            let mut slices = [IoSlice::new(&[]); MAX_WRITE_SLICES];
            let count = self.write_buf.fill_write_slices(&mut slices);
            if count == 0 {
                break;
            }

            let n = self.inner.write_vectored(&slices[..count]).await?;
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
            this.read_buf.reserve(crate::RECV_BUFFER_SIZE);
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

        // Reuse the message Vec across reads (no allocation per read). Messages
        // are popped from the back, so store them in reverse order.
        debug_assert!(self.pending_messages.is_empty());
        self.protocol
            .process_into(&mut self.read_buf, &mut self.pending_messages)?;
        self.pending_messages.reverse();

        Ok(())
    }

    /// Get the next pending message (moved out, no clone)
    #[inline]
    fn next_pending_message(&mut self) -> Option<Message> {
        self.pending_messages.pop()
    }

    /// Poll the heartbeat timer for `deadline_ms`, (re)arming it only when needed.
    ///
    /// Inbound traffic only ever moves the deadline later, so the timer is left
    /// untouched on every message and re-armed lazily when it fires. It is reset
    /// eagerly only when the deadline moves earlier (e.g. a Pong deadline starts).
    fn poll_heartbeat_timer(&mut self, cx: &mut Context<'_>, deadline_ms: u64) -> Poll<()> {
        let target = self.clock_epoch + Duration::from_millis(deadline_ms);
        match self.heartbeat_sleep.as_mut() {
            Some(sleep) => {
                if deadline_ms < self.heartbeat_armed_ms
                    || (deadline_ms != self.heartbeat_armed_ms && sleep.is_elapsed())
                {
                    sleep.as_mut().reset(target);
                    self.heartbeat_armed_ms = deadline_ms;
                }
            }
            None => {
                self.heartbeat_sleep = Some(Box::pin(tokio::time::sleep_until(target)));
                self.heartbeat_armed_ms = deadline_ms;
            }
        }
        self.heartbeat_sleep
            .as_mut()
            .expect("heartbeat timer armed above")
            .as_mut()
            .poll(cx)
    }

    /// Write every pending frame to the transport and flush it.
    fn poll_write_out(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.as_mut().get_mut();

        // Write all pending data
        while this.write_buf.has_data() {
            let mut slices = [IoSlice::new(&[]); MAX_WRITE_SLICES];
            let count = this.write_buf.fill_write_slices(&mut slices);
            if count == 0 {
                break;
            }

            match Pin::new(&mut this.inner).poll_write_vectored(cx, &slices[..count]) {
                Poll::Ready(Ok(0)) => {
                    this.state = StreamState::Closed;
                    this.heartbeat.stop();
                    this.heartbeat_sleep = None;
                    return Poll::Ready(Err(Error::ConnectionClosed));
                }
                Poll::Ready(Ok(n)) => {
                    this.write_buf.consume(n);
                }
                Poll::Ready(Err(e)) => {
                    this.state = StreamState::Closed;
                    this.heartbeat.stop();
                    this.heartbeat_sleep = None;
                    return Poll::Ready(Err(e.into()));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }

        // Flush underlying stream
        let this = self.as_mut().get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => {
                this.state = StreamState::Closed;
                this.heartbeat.stop();
                this.heartbeat_sleep = None;
                Poll::Ready(Err(e.into()))
            }
            Poll::Pending => Poll::Pending,
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
                match self.as_mut().poll_write_out(cx) {
                    Poll::Ready(Ok(())) => {
                        let this = self.as_mut().get_mut();
                        this.flush_on_read = false;
                        if this.ping_flush_pending {
                            this.ping_flush_pending = false;
                            let now = this.clock_epoch.elapsed().as_millis() as u64;
                            // The Pong deadline is earlier than the armed one; the
                            // timer is reset on the next poll.
                            this.heartbeat.ping_flushed(now);
                        }

                        if this.close_after_flush {
                            this.close_after_flush = false;
                            this.state = StreamState::Closed;
                        }

                        if let Some(error) = this.pending_terminal_error.take() {
                            return Poll::Ready(Some(Err(error)));
                        }
                        if let Some(msg) = this.pending_control_message.take() {
                            return Poll::Ready(Some(Ok(msg)));
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        let this = self.as_mut().get_mut();
                        this.state = StreamState::Closed;
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // Check for connection closed
            if self.state == StreamState::Closed {
                return Poll::Ready(None);
            }

            // Heartbeat deadlines are based on inbound inactivity. A Pong
            // deadline starts only after the corresponding Ping is flushed.
            let deadline = self.heartbeat.next_deadline();
            if let Some(deadline) = deadline {
                let now = self.clock_epoch.elapsed().as_millis() as u64;
                if deadline.at() <= now {
                    let this = self.as_mut().get_mut();
                    match deadline {
                        Deadline::Ping(_) => {
                            if let Some(payload) = this.heartbeat.ping_due(now) {
                                if let Err(e) = this.protocol.encode_message(
                                    &Message::Ping(payload),
                                    this.write_buf.buffer_mut(),
                                ) {
                                    this.state = StreamState::Closed;
                                    this.heartbeat.stop();
                                    return Poll::Ready(Some(Err(e)));
                                }
                                this.ping_flush_pending = true;
                                this.flush_on_read = true;
                            }
                        }
                        Deadline::Pong(_) | Deadline::Idle(_) => {
                            let (code, reason, error) = match deadline {
                                Deadline::Pong(_) => (
                                    this.config.pong_timeout_close_code,
                                    bounded_close_reason(&this.config.pong_timeout_close_reason),
                                    Error::HeartbeatTimeout,
                                ),
                                Deadline::Idle(_) => (
                                    CloseReason::GOING_AWAY,
                                    "Connection idle timeout".to_string(),
                                    Error::IdleTimeout,
                                ),
                                Deadline::Ping(_) => unreachable!(),
                            };
                            let close = Message::Close(Some(CloseReason::new(code, reason)));
                            if let Err(e) = this
                                .protocol
                                .encode_message(&close, this.write_buf.buffer_mut())
                            {
                                this.state = StreamState::Closed;
                                this.heartbeat.stop();
                                return Poll::Ready(Some(Err(e)));
                            }
                            this.heartbeat.stop();
                            this.state = StreamState::CloseSent;
                            this.pending_terminal_error = Some(error);
                            this.flush_on_read = true;
                            this.close_after_flush = true;
                        }
                    }
                    this.heartbeat_sleep = None;
                    continue;
                }

                if self
                    .as_mut()
                    .get_mut()
                    .poll_heartbeat_timer(cx, deadline.at())
                    .is_ready()
                {
                    // Fired: loop back to re-evaluate the (possibly moved) deadline.
                    continue;
                }
            }

            // First, return any pending messages
            if let Some(msg) = self.as_mut().get_mut().next_pending_message() {
                let this = self.as_mut().get_mut();
                let now = this.clock_epoch.elapsed().as_millis() as u64;
                let pong = match &msg {
                    Message::Pong(payload) => Some(payload),
                    _ => None,
                };
                // Inbound traffic only pushes deadlines later; the armed timer is
                // left alone and re-armed lazily when it fires.
                this.heartbeat.on_inbound(now, pong);

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
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
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

            // Process handshake leftover once before waiting for transport data.
            if self.has_unprocessed_read_data {
                self.as_mut().get_mut().has_unprocessed_read_data = false;
                match self.as_mut().get_mut().process_read_buf() {
                    Ok(()) if !self.pending_messages.is_empty() => continue,
                    Ok(()) => {}
                    Err(e) => {
                        let this = self.as_mut().get_mut();
                        this.state = StreamState::Closed;
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                }
            }

            // Try to read more data
            // Write out frames coalesced from earlier sends before waiting on
            // the transport, so batch-scoped corking never delays a reply past
            // the end of the read batch.
            if self.write_buf.has_data() {
                match self.as_mut().poll_write_out(cx) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(e)) => {
                        let this = self.as_mut().get_mut();
                        this.state = StreamState::Closed;
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            match self.as_mut().poll_read_more(cx) {
                Poll::Ready(Ok(0)) => {
                    // EOF - connection closed
                    self.as_mut().get_mut().state = StreamState::Closed;
                    self.as_mut().get_mut().heartbeat.stop();
                    return Poll::Ready(None);
                }
                Poll::Ready(Ok(_n)) => match self.as_mut().get_mut().process_read_buf() {
                    Ok(()) => continue,
                    Err(e) => {
                        let this = self.as_mut().get_mut();
                        this.state = StreamState::Closed;
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                },
                Poll::Ready(Err(e)) => {
                    let this = self.as_mut().get_mut();
                    this.state = StreamState::Closed;
                    this.heartbeat.stop();
                    this.heartbeat_sleep = None;
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
        if self.state != StreamState::Open {
            return Poll::Ready(Err(Error::ConnectionClosed));
        }
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<()> {
        let this = self.get_mut();

        if this.state != StreamState::Open {
            return Err(Error::ConnectionClosed);
        }

        // Track close frame sending
        if item.is_close() {
            this.state = StreamState::CloseSent;
            this.heartbeat.stop();
            this.heartbeat_sleep = None;
        }

        // Large unmasked data payloads are queued by reference behind their
        // header instead of being copied into the cork buffer; the vectored
        // write picks both up in order.
        if this.protocol.role == Role::Server {
            match &item {
                Message::Text(payload) | Message::Binary(payload)
                    if payload.len() >= crate::cork::ZERO_COPY_MIN =>
                {
                    let opcode = if item.is_text() {
                        OpCode::Text
                    } else {
                        OpCode::Binary
                    };
                    encode_frame_header(
                        this.write_buf.buffer_mut(),
                        opcode,
                        payload.len(),
                        true,
                        None,
                    );
                    this.write_buf.push_segment(payload.clone());
                    return Ok(());
                }
                _ => {}
            }
        }

        // Encode message into write buffer
        this.protocol
            .encode_message(&item, this.write_buf.buffer_mut())?;
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        {
            let this = self.as_mut().get_mut();
            // Batch-scoped corking: while inbound messages that were already
            // parsed are still queued for the application, keep the encoded
            // frames buffered. poll_next writes them all in one vectored write
            // before it next waits on the transport, so a read batch answered
            // with N sends costs one syscall instead of N.
            if this.config.write_coalescing
                && this.state == StreamState::Open
                && !this.pending_messages.is_empty()
                && this.write_buf.pending_bytes() < this.high_water_mark
            {
                return Poll::Ready(Ok(()));
            }
        }
        self.poll_write_out(cx)
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
        match self.as_mut().poll_write_out(cx) {
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
// Split stream implementation
// ============================================================================
//
// One background driver owns the transport writer. Both application writes and
// RFC control work use bounded queues; this is what lets Ping/Pong/Close make
// progress when the application performs zero writes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

const SPLIT_CONTROL_CAPACITY: usize = 32;
const SPLIT_OPEN: u8 = 0;
const SPLIT_CLOSING: u8 = 1;
const SPLIT_CLOSED: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalCause {
    ConnectionClosed,
    HeartbeatTimeout,
    IdleTimeout,
}

impl TerminalCause {
    fn error(self) -> Error {
        match self {
            Self::ConnectionClosed => Error::ConnectionClosed,
            Self::HeartbeatTimeout => Error::HeartbeatTimeout,
            Self::IdleTimeout => Error::IdleTimeout,
        }
    }
}

#[derive(Debug)]
enum ControlRequest {
    Ping(Bytes, tokio::time::Instant),
    Pong(Bytes, tokio::time::Instant),
    PeerClose,
    /// The application wrote a Close frame through the shared sink.
    LocalCloseSent,
    Eof,
}

struct SplitShared {
    status: AtomicU8,
    terminal_tx: watch::Sender<Option<TerminalCause>>,
    cancel: CancellationToken,
    /// Clock epoch shared by the reader and the writer driver
    epoch: tokio::time::Instant,
    /// Milliseconds since `epoch` of the last inbound data frame (reader -> driver)
    last_inbound_ms: AtomicU64,
}

impl SplitShared {
    fn new(closed: bool) -> Arc<Self> {
        let (terminal_tx, _) = watch::channel(closed.then_some(TerminalCause::ConnectionClosed));
        Arc::new(Self {
            status: AtomicU8::new(if closed { SPLIT_CLOSED } else { SPLIT_OPEN }),
            terminal_tx,
            cancel: CancellationToken::new(),
            epoch: tokio::time::Instant::now(),
            last_inbound_ms: AtomicU64::new(0),
        })
    }

    #[inline]
    fn note_inbound(&self) {
        let now_ms = self.epoch.elapsed().as_millis() as u64;
        self.last_inbound_ms.fetch_max(now_ms, Ordering::Relaxed);
    }

    fn begin_closing(&self) -> bool {
        self.status
            .compare_exchange(
                SPLIT_OPEN,
                SPLIT_CLOSING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn terminate(&self, cause: TerminalCause) {
        if self.status.swap(SPLIT_CLOSED, Ordering::AcqRel) != SPLIT_CLOSED {
            self.terminal_tx.send_replace(Some(cause));
        }
    }

    fn is_open(&self) -> bool {
        self.status.load(Ordering::Acquire) == SPLIT_OPEN
    }
}

trait SplitEncoder: 'static {
    fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()>;
    fn encode_pong(&mut self, payload: &[u8], buf: &mut BytesMut);
    fn encode_close_response(&mut self, buf: &mut BytesMut);
}

/// The transport writer and its encoder, shared between the application's
/// write handle and the connection's control driver.
///
/// Each side locks it only for one frame write, so application frames and
/// automatic Pong/Ping/Close frames interleave at frame boundaries without a
/// channel hop or a task wakeup per message. An uncontended lock is an atomic
/// operation and allocates nothing.
struct SplitSink<W, E> {
    writer: W,
    encoder: E,
    buf: BytesMut,
}

type SharedSink<W, E> = Arc<tokio::sync::Mutex<SplitSink<W, E>>>;

impl<W, E> SplitSink<W, E>
where
    W: AsyncWrite + Unpin,
    E: SplitEncoder,
{
    fn new(writer: W, encoder: E, capacity: usize) -> Self {
        Self {
            writer,
            encoder,
            buf: BytesMut::with_capacity(capacity),
        }
    }

    /// Encode one frame with `encode` and write it out, unless cancelled.
    async fn write_frame(
        &mut self,
        cancel: &CancellationToken,
        encode: impl FnOnce(&mut E, &mut BytesMut) -> Result<()>,
    ) -> Result<()> {
        self.buf.clear();
        encode(&mut self.encoder, &mut self.buf)?;
        let result = write_split_bytes(&mut self.writer, &self.buf, cancel).await;
        self.buf.clear();
        result
    }
}

/// Application write handle shared by the plain and compressed split writers.
struct SplitWriterCore<W, E> {
    sink: SharedSink<W, E>,
    control_tx: mpsc::Sender<ControlRequest>,
    shared: Arc<SplitShared>,
}

impl<W, E> SplitWriterCore<W, E>
where
    W: AsyncWrite + Unpin,
    E: SplitEncoder,
{
    async fn send(&self, msg: Message) -> Result<()> {
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        let mut sink = self.sink.lock().await;
        // Re-check under the lock: the control driver may have closed meanwhile.
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        let is_close = msg.is_close();
        if is_close {
            self.shared.begin_closing();
        }
        let result = sink
            .write_frame(&self.shared.cancel, |encoder, buf| {
                encoder.encode_message(&msg, buf)
            })
            .await;
        drop(sink);
        if result.is_err() {
            self.shared.terminate(TerminalCause::ConnectionClosed);
            return result;
        }
        if is_close {
            let _ = self.control_tx.send(ControlRequest::LocalCloseSent).await;
        }
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        let mut sink = self.sink.lock().await;
        let result = tokio::select! {
            result = sink.writer.flush() => result.map_err(Into::into),
            _ = self.shared.cancel.cancelled() => Err(Error::ConnectionClosed),
        };
        drop(sink);
        if result.is_err() {
            self.shared.terminate(TerminalCause::ConnectionClosed);
        }
        result
    }

    fn is_closed(&self) -> bool {
        !self.shared.is_open()
    }

    fn current_error(&self) -> Error {
        self.shared
            .terminal_tx
            .borrow()
            .map_or(Error::ConnectionClosed, TerminalCause::error)
    }
}

impl SplitEncoder for Protocol {
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

/// The read half of a split WebSocket stream.
pub struct SplitReader<S> {
    reader: ReadHalf<S>,
    protocol: Protocol,
    read_buf: BytesMut,
    has_unprocessed_read_data: bool,
    pending_messages: Vec<Message>,
    control_tx: mpsc::Sender<ControlRequest>,
    terminal_rx: watch::Receiver<Option<TerminalCause>>,
    shared: Arc<SplitShared>,
    terminal_reported: bool,
}

/// The write half of a split WebSocket stream.
///
/// The transport writer itself is owned by the per-connection control driver.
pub struct SplitWriter<S> {
    core: SplitWriterCore<WriteHalf<S>, Protocol>,
}

impl<S> WebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// Split into concurrently usable read and write handles.
    ///
    /// This starts one connection-scoped Tokio task that exclusively owns the
    /// transport writer. Dropping either returned half cancels that task.
    pub fn split(self) -> (SplitReader<S>, SplitWriter<S>) {
        let (reader, writer) = tokio::io::split(self.inner);
        let (control_tx, control_rx) = mpsc::channel(SPLIT_CONTROL_CAPACITY);
        let shared = SplitShared::new(self.state != StreamState::Open);
        let terminal_rx = shared.terminal_tx.subscribe();
        let reader_protocol = Protocol::new(
            self.protocol.role,
            self.config.max_frame_size,
            self.config.max_message_size,
        );
        let sink: SharedSink<WriteHalf<S>, Protocol> = Arc::new(tokio::sync::Mutex::new(
            SplitSink::new(writer, self.protocol, self.config.write_buffer_size),
        ));

        tokio::spawn(split_writer_driver(
            sink.clone(),
            self.config,
            control_rx,
            shared.clone(),
        ));

        (
            SplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                has_unprocessed_read_data: self.has_unprocessed_read_data,
                pending_messages: self.pending_messages,
                control_tx: control_tx.clone(),
                terminal_rx,
                shared: shared.clone(),
                terminal_reported: false,
            },
            SplitWriter {
                core: SplitWriterCore {
                    sink,
                    control_tx,
                    shared,
                },
            },
        )
    }
}

impl<S> SplitReader<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Receive the next message.
    ///
    /// Ping and Pong frames remain visible after their automatic state-machine
    /// processing. A terminal heartbeat/idle cause is yielded once as an error.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        loop {
            if let Some(result) = self.take_terminal() {
                return result;
            }

            if let Some(msg) = self.pending_messages.pop() {
                let request = match &msg {
                    Message::Ping(data) => {
                        ControlRequest::Ping(data.clone(), tokio::time::Instant::now())
                    }
                    Message::Pong(data) => {
                        ControlRequest::Pong(data.clone(), tokio::time::Instant::now())
                    }
                    Message::Close(_) => {
                        self.shared.begin_closing();
                        ControlRequest::PeerClose
                    }
                    _ => {
                        // Data frames only need to refresh the inactivity clock; a
                        // relaxed store avoids a channel round trip per message.
                        self.shared.note_inbound();
                        return Some(Ok(msg));
                    }
                };
                if self.control_tx.send(request).await.is_err() {
                    self.shared.terminate(TerminalCause::ConnectionClosed);
                    continue;
                }
                return Some(Ok(msg));
            }

            if self.has_unprocessed_read_data {
                self.has_unprocessed_read_data = false;
                debug_assert!(self.pending_messages.is_empty());
                match self
                    .protocol
                    .process_into(&mut self.read_buf, &mut self.pending_messages)
                {
                    Ok(()) => {
                        self.pending_messages.reverse();
                        if !self.pending_messages.is_empty() {
                            continue;
                        }
                    }
                    Err(error) => {
                        self.shared.terminate(TerminalCause::ConnectionClosed);
                        return Some(Err(error));
                    }
                }
            }

            if self.read_buf.capacity() - self.read_buf.len() < 4096 {
                self.read_buf.reserve(crate::RECV_BUFFER_SIZE);
            }

            tokio::select! {
                biased;
                changed = self.terminal_rx.changed() => {
                    if changed.is_err() {
                        self.shared.terminate(TerminalCause::ConnectionClosed);
                    }
                }
                result = self.reader.read_buf(&mut self.read_buf) => {
                    match result {
                        Ok(0) => {
                            let _ = self.control_tx.send(ControlRequest::Eof).await;
                            self.shared.terminate(TerminalCause::ConnectionClosed);
                        }
                        Ok(_) => match self
                            .protocol
                            .process_into(&mut self.read_buf, &mut self.pending_messages)
                        {
                            Ok(()) => self.pending_messages.reverse(),
                            Err(error) => {
                                self.shared.terminate(TerminalCause::ConnectionClosed);
                                return Some(Err(error));
                            }
                        },
                        Err(error) => {
                            self.shared.terminate(TerminalCause::ConnectionClosed);
                            return Some(Err(error.into()));
                        }
                    }
                }
            }
        }
    }

    fn take_terminal(&mut self) -> Option<Option<Result<Message>>> {
        if self.shared.status.load(Ordering::Acquire) != SPLIT_CLOSED {
            return None;
        }
        if self.terminal_reported {
            return Some(None);
        }
        self.terminal_reported = true;
        match *self.terminal_rx.borrow() {
            Some(TerminalCause::HeartbeatTimeout) => Some(Some(Err(Error::HeartbeatTimeout))),
            Some(TerminalCause::IdleTimeout) => Some(Some(Err(Error::IdleTimeout))),
            _ => Some(None),
        }
    }

    /// Check whether the connection is closing or closed.
    pub fn is_closed(&self) -> bool {
        !self.shared.is_open()
    }
}

impl<S> Drop for SplitReader<S> {
    fn drop(&mut self) {
        self.shared.cancel.cancel();
    }
}

impl<S> SplitWriter<S>
where
    S: AsyncWrite + Unpin,
{
    /// Send a message.
    ///
    /// The frame is written directly to the transport; automatic Pong, Ping
    /// and Close frames from the connection driver interleave at frame
    /// boundaries.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        self.core.send(msg).await
    }

    /// Send a text message.
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.send(Message::text(text)).await
    }

    /// Send a binary message.
    pub async fn send_binary(&mut self, data: Bytes) -> Result<()> {
        self.send(Message::Binary(data)).await
    }

    /// Send a local Close frame.
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        self.send(Message::Close(Some(CloseReason::new(code, reason))))
            .await
    }

    /// Flush the transport.
    pub async fn flush(&mut self) -> Result<()> {
        self.core.flush().await
    }

    /// Check whether the connection is closing or closed.
    pub fn is_closed(&self) -> bool {
        self.core.is_closed()
    }
}

impl<S> Drop for SplitWriter<S> {
    fn drop(&mut self) {
        self.core.shared.cancel.cancel();
    }
}

async fn split_writer_driver<W, E>(
    sink: SharedSink<W, E>,
    config: Config,
    mut control_rx: mpsc::Receiver<ControlRequest>,
    shared: Arc<SplitShared>,
) where
    W: AsyncWrite + Unpin,
    E: SplitEncoder,
{
    let epoch = shared.epoch;
    let mut heartbeat = Heartbeat::new(&config, 0);
    let mut closing_deadline = None;
    let mut local_close_sent = false;

    // One timer for the whole connection: re-armed lazily when it fires or when
    // the deadline moves earlier, never per message.
    let mut heartbeat_sleep = Box::pin(tokio::time::sleep_until(epoch + Duration::from_secs(3600)));
    let mut heartbeat_armed_ms: Option<u64> = None;
    let mut last_synced_inbound_ms = 0u64;

    loop {
        // Pick up data-frame activity published by the reader without a channel.
        let observed_inbound = shared.last_inbound_ms.load(Ordering::Relaxed);
        if observed_inbound > last_synced_inbound_ms {
            last_synced_inbound_ms = observed_inbound;
            heartbeat.on_inbound(observed_inbound, None);
        }

        let heartbeat_deadline = heartbeat.next_deadline();
        if let Some(deadline) = heartbeat_deadline {
            let at = deadline.at();
            let rearm = match heartbeat_armed_ms {
                None => true,
                Some(armed) => at < armed || (at != armed && heartbeat_sleep.is_elapsed()),
            };
            if rearm {
                heartbeat_sleep
                    .as_mut()
                    .reset(epoch + Duration::from_millis(at));
                heartbeat_armed_ms = Some(at);
            }
        }
        let close_delay = closing_deadline
            .map(|deadline: tokio::time::Instant| {
                deadline.saturating_duration_since(tokio::time::Instant::now())
            })
            .unwrap_or(Duration::from_secs(365 * 24 * 60 * 60));

        tokio::select! {
            biased;
            _ = shared.cancel.cancelled() => {
                shared.terminate(TerminalCause::ConnectionClosed);
                break;
            }
            request = control_rx.recv() => {
                let Some(request) = request else {
                    shared.terminate(TerminalCause::ConnectionClosed);
                    break;
                };
                match request {
                    ControlRequest::Ping(payload, received_at) => {
                        let received_ms =
                            received_at.saturating_duration_since(epoch).as_millis() as u64;
                        heartbeat.on_inbound(received_ms, None);
                        let written = sink
                            .lock()
                            .await
                            .write_frame(&shared.cancel, |encoder, buf| {
                                encoder.encode_pong(&payload, buf);
                                Ok(())
                            })
                            .await;
                        if written.is_err() {
                            shared.terminate(TerminalCause::ConnectionClosed);
                            break;
                        }
                    }
                    ControlRequest::Pong(payload, received_at) => {
                        let received_ms =
                            received_at.saturating_duration_since(epoch).as_millis() as u64;
                        heartbeat.on_inbound(received_ms, Some(&payload));
                    }
                    ControlRequest::PeerClose => {
                        heartbeat.stop();
                        let mut guard = sink.lock().await;
                        if !local_close_sent {
                            let _ = guard
                                .write_frame(&shared.cancel, |encoder, buf| {
                                    encoder.encode_close_response(buf);
                                    Ok(())
                                })
                                .await;
                        }
                        let _ = bounded_shutdown(&mut guard.writer, config.close_timeout).await;
                        drop(guard);
                        shared.terminate(TerminalCause::ConnectionClosed);
                        break;
                    }
                    ControlRequest::LocalCloseSent => {
                        heartbeat.stop();
                        local_close_sent = true;
                        closing_deadline = Some(
                            tokio::time::Instant::now()
                                + Duration::from_secs(config.close_timeout.into()),
                        );
                    }
                    ControlRequest::Eof => {
                        heartbeat.stop();
                        shared.terminate(TerminalCause::ConnectionClosed);
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(close_delay), if closing_deadline.is_some() => {
                let _ = bounded_shutdown(&mut sink.lock().await.writer, config.close_timeout).await;
                shared.terminate(TerminalCause::ConnectionClosed);
                break;
            }
            _ = &mut heartbeat_sleep, if heartbeat_deadline.is_some() && shared.is_open() => {
                let now_ms = epoch.elapsed().as_millis() as u64;
                match heartbeat.next_deadline() {
                    Some(Deadline::Ping(at)) if at <= now_ms => {
                        if let Some(payload) = heartbeat.ping_due(now_ms) {
                            let written = sink
                                .lock()
                                .await
                                .write_frame(&shared.cancel, |encoder, buf| {
                                    encoder.encode_message(&Message::Ping(payload), buf)
                                })
                                .await;
                            if written.is_err() {
                                shared.terminate(TerminalCause::ConnectionClosed);
                                break;
                            }
                            heartbeat.ping_flushed(epoch.elapsed().as_millis() as u64);
                        }
                    }
                    Some(Deadline::Pong(at)) if at <= now_ms => {
                        timeout_close(
                            &sink,
                            &config,
                            config.pong_timeout_close_code,
                            &config.pong_timeout_close_reason,
                            &shared.cancel,
                        ).await;
                        shared.terminate(TerminalCause::HeartbeatTimeout);
                        break;
                    }
                    Some(Deadline::Idle(at)) if at <= now_ms => {
                        timeout_close(
                            &sink,
                            &config,
                            CloseReason::GOING_AWAY,
                            "Connection idle timeout",
                            &shared.cancel,
                        ).await;
                        shared.terminate(TerminalCause::IdleTimeout);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn write_split_bytes<W>(
    writer: &mut W,
    bytes: &[u8],
    cancel: &CancellationToken,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    tokio::select! {
        result = async {
            writer.write_all(bytes).await?;
            writer.flush().await?;
            Ok::<(), std::io::Error>(())
        } => result.map_err(Into::into),
        _ = cancel.cancelled() => Err(Error::ConnectionClosed),
    }
}

async fn bounded_shutdown<W>(writer: &mut W, seconds: u32) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    tokio::time::timeout(Duration::from_secs(seconds.into()), async {
        writer.flush().await?;
        writer.shutdown().await
    })
    .await
    .map_err(|_| Error::ConnectionClosed)?
    .map_err(Into::into)
}

async fn timeout_close<W, E>(
    sink: &SharedSink<W, E>,
    config: &Config,
    code: u16,
    reason: &str,
    cancel: &CancellationToken,
) where
    W: AsyncWrite + Unpin,
    E: SplitEncoder,
{
    let mut guard = sink.lock().await;
    let close = Message::Close(Some(CloseReason::new(code, bounded_close_reason(reason))));
    let _ = tokio::time::timeout(
        Duration::from_secs(config.close_timeout.into()),
        guard.write_frame(cancel, |encoder, buf| encoder.encode_message(&close, buf)),
    )
    .await;
    let _ = bounded_shutdown(&mut guard.writer, config.close_timeout).await;
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
        // Deadline (ms since clock_epoch) the heartbeat timer is currently armed for
        heartbeat_armed_ms: u64,
        pending_control_message: Option<Message>,
        pending_terminal_error: Option<Error>,
        flush_on_read: bool,
        close_after_flush: bool,
        ping_flush_pending: bool,
        clock_epoch: tokio::time::Instant,
        heartbeat: Heartbeat,
        heartbeat_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
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

        let clock_epoch = tokio::time::Instant::now();
        let heartbeat = Heartbeat::new(&config, 0);
        Self {
            inner,
            protocol,
            read_buf: BytesMut::with_capacity(crate::RECV_BUFFER_SIZE),
            write_buf: CorkBuffer::with_capacity(config.write_buffer_size),
            state: StreamState::Open,
            config,
            pending_messages: Vec::new(),
            heartbeat_armed_ms: 0,
            pending_control_message: None,
            pending_terminal_error: None,
            flush_on_read: false,
            close_after_flush: false,
            ping_flush_pending: false,
            clock_epoch,
            heartbeat,
            heartbeat_sleep: None,
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

        let clock_epoch = tokio::time::Instant::now();
        let heartbeat = Heartbeat::new(&config, 0);
        Self {
            inner,
            protocol,
            read_buf: BytesMut::with_capacity(crate::RECV_BUFFER_SIZE),
            write_buf: CorkBuffer::with_capacity(config.write_buffer_size),
            state: StreamState::Open,
            config,
            pending_messages: Vec::new(),
            heartbeat_armed_ms: 0,
            pending_control_message: None,
            pending_terminal_error: None,
            flush_on_read: false,
            close_after_flush: false,
            ping_flush_pending: false,
            clock_epoch,
            heartbeat,
            heartbeat_sleep: None,
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
            let mut slices = [IoSlice::new(&[]); MAX_WRITE_SLICES];
            let count = self.write_buf.fill_write_slices(&mut slices);
            if count == 0 {
                break;
            }

            let n = self.inner.write_vectored(&slices[..count]).await?;
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
            this.read_buf.reserve(crate::RECV_BUFFER_SIZE);
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

        // Reuse the message Vec across reads (no allocation per read). Messages
        // are popped from the back, so store them in reverse order.
        debug_assert!(self.pending_messages.is_empty());
        self.protocol
            .process_into(&mut self.read_buf, &mut self.pending_messages)?;
        self.pending_messages.reverse();

        Ok(())
    }

    /// Get the next pending message (moved out, no clone)
    #[inline]
    fn next_pending_message(&mut self) -> Option<Message> {
        self.pending_messages.pop()
    }

    /// See [`WebSocketStream::poll_heartbeat_timer`].
    fn poll_heartbeat_timer(&mut self, cx: &mut Context<'_>, deadline_ms: u64) -> Poll<()> {
        let target = self.clock_epoch + Duration::from_millis(deadline_ms);
        match self.heartbeat_sleep.as_mut() {
            Some(sleep) => {
                if deadline_ms < self.heartbeat_armed_ms
                    || (deadline_ms != self.heartbeat_armed_ms && sleep.is_elapsed())
                {
                    sleep.as_mut().reset(target);
                    self.heartbeat_armed_ms = deadline_ms;
                }
            }
            None => {
                self.heartbeat_sleep = Some(Box::pin(tokio::time::sleep_until(target)));
                self.heartbeat_armed_ms = deadline_ms;
            }
        }
        self.heartbeat_sleep
            .as_mut()
            .expect("heartbeat timer armed above")
            .as_mut()
            .poll(cx)
    }

    /// Write every pending frame to the transport and flush it.
    fn poll_write_out(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.as_mut().get_mut();

        while this.write_buf.has_data() {
            let mut slices = [IoSlice::new(&[]); MAX_WRITE_SLICES];
            let count = this.write_buf.fill_write_slices(&mut slices);
            if count == 0 {
                break;
            }

            match Pin::new(&mut this.inner).poll_write_vectored(cx, &slices[..count]) {
                Poll::Ready(Ok(0)) => {
                    this.state = StreamState::Closed;
                    this.heartbeat.stop();
                    this.heartbeat_sleep = None;
                    return Poll::Ready(Err(Error::ConnectionClosed));
                }
                Poll::Ready(Ok(n)) => {
                    this.write_buf.consume(n);
                }
                Poll::Ready(Err(e)) => {
                    this.state = StreamState::Closed;
                    this.heartbeat.stop();
                    this.heartbeat_sleep = None;
                    return Poll::Ready(Err(e.into()));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }

        let this = self.as_mut().get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => {
                this.state = StreamState::Closed;
                this.heartbeat.stop();
                this.heartbeat_sleep = None;
                Poll::Ready(Err(e.into()))
            }
            Poll::Pending => Poll::Pending,
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
                match self.as_mut().poll_write_out(cx) {
                    Poll::Ready(Ok(())) => {
                        let this = self.as_mut().get_mut();
                        this.flush_on_read = false;
                        if this.ping_flush_pending {
                            this.ping_flush_pending = false;
                            let now = this.clock_epoch.elapsed().as_millis() as u64;
                            // The Pong deadline is earlier than the armed one; the
                            // timer is reset on the next poll.
                            this.heartbeat.ping_flushed(now);
                        }

                        if this.close_after_flush {
                            this.close_after_flush = false;
                            this.state = StreamState::Closed;
                        }

                        if let Some(error) = this.pending_terminal_error.take() {
                            return Poll::Ready(Some(Err(error)));
                        }
                        if let Some(msg) = this.pending_control_message.take() {
                            return Poll::Ready(Some(Ok(msg)));
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        let this = self.as_mut().get_mut();
                        this.state = StreamState::Closed;
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            if self.state == StreamState::Closed {
                return Poll::Ready(None);
            }

            let deadline = self.heartbeat.next_deadline();
            if let Some(deadline) = deadline {
                let now = self.clock_epoch.elapsed().as_millis() as u64;
                if deadline.at() <= now {
                    let this = self.as_mut().get_mut();
                    match deadline {
                        Deadline::Ping(_) => {
                            if let Some(payload) = this.heartbeat.ping_due(now) {
                                if let Err(e) = this.protocol.encode_message(
                                    &Message::Ping(payload),
                                    this.write_buf.buffer_mut(),
                                ) {
                                    this.state = StreamState::Closed;
                                    this.heartbeat.stop();
                                    return Poll::Ready(Some(Err(e)));
                                }
                                this.ping_flush_pending = true;
                                this.flush_on_read = true;
                            }
                        }
                        Deadline::Pong(_) | Deadline::Idle(_) => {
                            let (code, reason, error) = match deadline {
                                Deadline::Pong(_) => (
                                    this.config.pong_timeout_close_code,
                                    bounded_close_reason(&this.config.pong_timeout_close_reason),
                                    Error::HeartbeatTimeout,
                                ),
                                Deadline::Idle(_) => (
                                    CloseReason::GOING_AWAY,
                                    "Connection idle timeout".to_string(),
                                    Error::IdleTimeout,
                                ),
                                Deadline::Ping(_) => unreachable!(),
                            };
                            let close = Message::Close(Some(CloseReason::new(code, reason)));
                            if let Err(e) = this
                                .protocol
                                .encode_message(&close, this.write_buf.buffer_mut())
                            {
                                this.state = StreamState::Closed;
                                this.heartbeat.stop();
                                return Poll::Ready(Some(Err(e)));
                            }
                            this.heartbeat.stop();
                            this.state = StreamState::CloseSent;
                            this.pending_terminal_error = Some(error);
                            this.flush_on_read = true;
                            this.close_after_flush = true;
                        }
                    }
                    this.heartbeat_sleep = None;
                    continue;
                }

                if self
                    .as_mut()
                    .get_mut()
                    .poll_heartbeat_timer(cx, deadline.at())
                    .is_ready()
                {
                    // Fired: loop back to re-evaluate the (possibly moved) deadline.
                    continue;
                }
            }

            if let Some(msg) = self.as_mut().get_mut().next_pending_message() {
                let this = self.as_mut().get_mut();
                let now = this.clock_epoch.elapsed().as_millis() as u64;
                let pong = match &msg {
                    Message::Pong(payload) => Some(payload),
                    _ => None,
                };
                // Inbound traffic only pushes deadlines later; the armed timer is
                // left alone and re-armed lazily when it fires.
                this.heartbeat.on_inbound(now, pong);

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
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
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

            // Write out frames coalesced from earlier sends before waiting on
            // the transport, so batch-scoped corking never delays a reply past
            // the end of the read batch.
            if self.write_buf.has_data() {
                match self.as_mut().poll_write_out(cx) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(e)) => {
                        let this = self.as_mut().get_mut();
                        this.state = StreamState::Closed;
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            match self.as_mut().poll_read_more(cx) {
                Poll::Ready(Ok(0)) => {
                    self.as_mut().get_mut().state = StreamState::Closed;
                    self.as_mut().get_mut().heartbeat.stop();
                    return Poll::Ready(None);
                }
                Poll::Ready(Ok(_n)) => match self.as_mut().get_mut().process_read_buf() {
                    Ok(()) => continue,
                    Err(e) => {
                        let this = self.as_mut().get_mut();
                        this.state = StreamState::Closed;
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                },
                Poll::Ready(Err(e)) => {
                    let this = self.as_mut().get_mut();
                    this.state = StreamState::Closed;
                    this.heartbeat.stop();
                    this.heartbeat_sleep = None;
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
        if self.state != StreamState::Open {
            return Poll::Ready(Err(Error::ConnectionClosed));
        }
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<()> {
        let this = self.get_mut();

        if this.state != StreamState::Open {
            return Err(Error::ConnectionClosed);
        }

        if item.is_close() {
            this.state = StreamState::CloseSent;
            this.heartbeat.stop();
            this.heartbeat_sleep = None;
        }

        this.protocol
            .encode_message(&item, this.write_buf.buffer_mut())?;
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        {
            let this = self.as_mut().get_mut();
            // Batch-scoped corking: while inbound messages that were already
            // parsed are still queued for the application, keep the encoded
            // frames buffered. poll_next writes them all in one vectored write
            // before it next waits on the transport, so a read batch answered
            // with N sends costs one syscall instead of N.
            if this.config.write_coalescing
                && this.state == StreamState::Open
                && !this.pending_messages.is_empty()
                && this.write_buf.pending_bytes() < this.high_water_mark
            {
                return Poll::Ready(Ok(()));
            }
        }
        self.poll_write_out(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        if self.state == StreamState::Open {
            let close = Message::Close(Some(CloseReason::new(1000, "")));
            if let Err(e) = self.as_mut().start_send(close) {
                return Poll::Ready(Err(e));
            }
        }

        match self.as_mut().poll_write_out(cx) {
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
    control_tx: mpsc::Sender<ControlRequest>,
    terminal_rx: watch::Receiver<Option<TerminalCause>>,
    shared: Arc<SplitShared>,
    terminal_reported: bool,
}

/// The write half of a split compressed WebSocket stream
///
/// Created by calling `split()` on a `CompressedWebSocketStream`.
/// This half owns the write side of the TCP stream and can operate
/// completely independently from the read half.
#[cfg(feature = "permessage-deflate")]
pub struct CompressedSplitWriter<S> {
    core: SplitWriterCore<WriteHalf<S>, crate::protocol::CompressedWriterProtocol>,
}

#[cfg(feature = "permessage-deflate")]
impl SplitEncoder for crate::protocol::CompressedWriterProtocol {
    fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()> {
        crate::protocol::CompressedWriterProtocol::encode_message(self, msg, buf)
    }

    fn encode_pong(&mut self, payload: &[u8], buf: &mut BytesMut) {
        crate::protocol::CompressedWriterProtocol::encode_pong(self, payload, buf);
    }

    fn encode_close_response(&mut self, buf: &mut BytesMut) {
        crate::protocol::CompressedWriterProtocol::encode_close_response(self, buf);
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> CompressedWebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
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

        let (control_tx, control_rx) = mpsc::channel(SPLIT_CONTROL_CAPACITY);
        let shared = SplitShared::new(self.state != StreamState::Open);
        let terminal_rx = shared.terminal_tx.subscribe();

        // Split the protocol into reader and writer halves
        let (reader_protocol, writer_protocol) = self
            .protocol
            .split(self.config.max_frame_size, self.config.max_message_size);
        let sink = Arc::new(tokio::sync::Mutex::new(SplitSink::new(
            writer,
            writer_protocol,
            self.config.write_buffer_size,
        )));

        tokio::spawn(split_writer_driver(
            sink.clone(),
            self.config,
            control_rx,
            shared.clone(),
        ));

        (
            CompressedSplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                pending_messages: self.pending_messages,
                control_tx: control_tx.clone(),
                terminal_rx,
                shared: shared.clone(),
                terminal_reported: false,
            },
            CompressedSplitWriter {
                core: SplitWriterCore {
                    sink,
                    control_tx,
                    shared,
                },
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
            if let Some(result) = self.take_terminal() {
                return result;
            }

            if let Some(msg) = self.pending_messages.pop() {
                let request = match &msg {
                    Message::Ping(data) => {
                        ControlRequest::Ping(data.clone(), tokio::time::Instant::now())
                    }
                    Message::Pong(data) => {
                        ControlRequest::Pong(data.clone(), tokio::time::Instant::now())
                    }
                    Message::Close(_) => {
                        self.shared.begin_closing();
                        ControlRequest::PeerClose
                    }
                    _ => {
                        // Data frames only need to refresh the inactivity clock; a
                        // relaxed store avoids a channel round trip per message.
                        self.shared.note_inbound();
                        return Some(Ok(msg));
                    }
                };
                if self.control_tx.send(request).await.is_err() {
                    self.shared.terminate(TerminalCause::ConnectionClosed);
                    continue;
                }
                return Some(Ok(msg));
            }

            if self.read_buf.capacity() - self.read_buf.len() < 4096 {
                self.read_buf.reserve(crate::RECV_BUFFER_SIZE);
            }

            tokio::select! {
                biased;
                changed = self.terminal_rx.changed() => {
                    if changed.is_err() {
                        self.shared.terminate(TerminalCause::ConnectionClosed);
                    }
                }
                result = self.reader.read_buf(&mut self.read_buf) => {
                    match result {
                        Ok(0) => {
                            let _ = self.control_tx.send(ControlRequest::Eof).await;
                            self.shared.terminate(TerminalCause::ConnectionClosed);
                        }
                        Ok(_) => match self
                            .protocol
                            .process_into(&mut self.read_buf, &mut self.pending_messages)
                        {
                            Ok(()) => self.pending_messages.reverse(),
                            Err(error) => {
                                self.shared.terminate(TerminalCause::ConnectionClosed);
                                return Some(Err(error));
                            }
                        },
                        Err(error) => {
                            self.shared.terminate(TerminalCause::ConnectionClosed);
                            return Some(Err(error.into()));
                        }
                    }
                }
            }
        }
    }

    fn take_terminal(&mut self) -> Option<Option<Result<Message>>> {
        if self.shared.status.load(Ordering::Acquire) != SPLIT_CLOSED {
            return None;
        }
        if self.terminal_reported {
            return Some(None);
        }
        self.terminal_reported = true;
        match *self.terminal_rx.borrow() {
            Some(TerminalCause::HeartbeatTimeout) => Some(Some(Err(Error::HeartbeatTimeout))),
            Some(TerminalCause::IdleTimeout) => Some(Some(Err(Error::IdleTimeout))),
            _ => Some(None),
        }
    }

    pub fn is_closed(&self) -> bool {
        !self.shared.is_open()
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> Drop for CompressedSplitReader<S> {
    fn drop(&mut self) {
        self.shared.cancel.cancel();
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> CompressedSplitWriter<S>
where
    S: AsyncWrite + Unpin,
{
    /// Send a message (compressed when the negotiated parameters say so).
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        self.core.send(msg).await
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

    /// Check whether the connection is closing or closed.
    pub fn is_closed(&self) -> bool {
        self.core.is_closed()
    }

    /// Flush the transport.
    pub async fn flush(&mut self) -> Result<()> {
        self.core.flush().await
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> Drop for CompressedSplitWriter<S> {
    fn drop(&mut self) {
        self.core.shared.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn read_masked_control_payload(
        io: &mut tokio::io::DuplexStream,
        expected_opcode: u8,
    ) -> Vec<u8> {
        let mut header = [0; 2];
        io.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], 0x80 | expected_opcode);
        assert_ne!(header[1] & 0x80, 0, "client frames must be masked");

        let payload_len = usize::from(header[1] & 0x7f);
        let mut mask = [0; 4];
        io.read_exact(&mut mask).await.unwrap();
        let mut payload = vec![0; payload_len];
        io.read_exact(&mut payload).await.unwrap();
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        payload
    }

    async fn read_masked_control_frame(
        io: &mut tokio::io::DuplexStream,
        expected_opcode: u8,
        expected_payload: &[u8],
    ) {
        let payload = read_masked_control_payload(io, expected_opcode).await;
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

    #[tokio::test(start_paused = true)]
    async fn auto_ping_uses_configured_interval_on_read_path() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let config = Config::builder()
            .auto_ping(true)
            .ping_interval(1)
            .idle_timeout(0)
            .build();
        let mut ws = WebSocketStream::client(client_io, config);

        let read_task = tokio::spawn(async move { ws.next().await });
        tokio::time::advance(Duration::from_secs(1)).await;
        let payload = read_masked_control_payload(&mut server_io, 0x09).await;
        assert_eq!(payload.len(), 8);
        read_task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn split_driver_pings_without_application_writes_and_correlates_pong() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let config = Config::builder()
            .ping_interval(1)
            .pong_timeout(1)
            .idle_timeout(0)
            .build();
        let ws = WebSocketStream::client(client_io, config);
        let (mut reader, _writer) = ws.split();

        tokio::time::advance(Duration::from_secs(1)).await;
        let first_ping = read_masked_control_payload(&mut server_io, 0x09).await;
        assert_eq!(first_ping.len(), 8);

        let mut pong = vec![0x8a, first_ping.len() as u8];
        pong.extend_from_slice(&first_ping);
        server_io.write_all(&pong).await.unwrap();
        assert!(matches!(
            reader.next().await,
            Some(Ok(Message::Pong(payload))) if payload == first_ping
        ));

        tokio::time::advance(Duration::from_secs(1)).await;
        let second_ping = read_masked_control_payload(&mut server_io, 0x09).await;
        assert_eq!(second_ping.len(), 8);
        assert_ne!(first_ping, second_ping);
    }

    #[tokio::test(start_paused = true)]
    async fn split_driver_times_out_with_configured_close_and_typed_cause() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let config = Config::builder()
            .ping_interval(1)
            .pong_timeout(1)
            .idle_timeout(0)
            .close_timeout(1)
            .pong_timeout_close(4201, "Pong reply not received in time")
            .build();
        let ws = WebSocketStream::client(client_io, config);
        let (mut reader, mut writer) = ws.split();

        tokio::time::advance(Duration::from_secs(1)).await;
        let _ = read_masked_control_payload(&mut server_io, 0x09).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let close = read_masked_control_payload(&mut server_io, 0x08).await;
        assert_eq!(u16::from_be_bytes([close[0], close[1]]), 4201);
        assert_eq!(&close[2..], b"Pong reply not received in time");

        assert!(matches!(
            reader.next().await,
            Some(Err(Error::HeartbeatTimeout))
        ));
        assert!(matches!(
            writer.send_text("too late").await,
            Err(Error::HeartbeatTimeout)
        ));
    }

    #[tokio::test]
    async fn split_driver_replies_to_peer_ping_without_writer_activity() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let ws = WebSocketStream::client(client_io, config);
        let (mut reader, _writer) = ws.split();

        server_io
            .write_all(&[0x89, 0x03, b'p', b'i', b'n'])
            .await
            .unwrap();
        assert!(matches!(
            reader.next().await,
            Some(Ok(Message::Ping(payload))) if payload == b"pin"[..]
        ));
        read_masked_control_frame(&mut server_io, 0x0a, b"pin").await;
    }

    #[tokio::test]
    async fn split_driver_replies_to_peer_close_without_writer_activity() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let ws = WebSocketStream::client(client_io, config);
        let (mut reader, mut writer) = ws.split();

        server_io
            .write_all(&[0x88, 0x02, 0x03, 0xe8])
            .await
            .unwrap();
        assert!(matches!(
            reader.next().await,
            Some(Ok(Message::Close(Some(reason)))) if reason.code == 1000
        ));
        read_masked_control_frame(&mut server_io, 0x08, b"").await;
        assert!(matches!(
            writer.send_text("too late").await,
            Err(Error::ConnectionClosed)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn split_hard_idle_timeout_is_typed_and_closes_once() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let config = Config::builder()
            .auto_ping(false)
            .idle_timeout(1)
            .close_timeout(1)
            .build();
        let ws = WebSocketStream::client(client_io, config);
        let (mut reader, mut writer) = ws.split();

        tokio::time::advance(Duration::from_secs(1)).await;
        let close = read_masked_control_payload(&mut server_io, 0x08).await;
        assert_eq!(
            u16::from_be_bytes([close[0], close[1]]),
            CloseReason::GOING_AWAY
        );
        assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
        assert!(matches!(
            writer.send_text("after idle").await,
            Err(Error::IdleTimeout)
        ));
        assert!(reader.next().await.is_none());
    }

    #[tokio::test]
    async fn dropping_either_split_half_cancels_the_connection() {
        let (client_io, _peer_io) = tokio::io::duplex(64);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let (reader, mut writer) = WebSocketStream::client(client_io, config).split();
        drop(reader);
        tokio::task::yield_now().await;
        assert!(matches!(
            writer.send_text("cancelled").await,
            Err(Error::ConnectionClosed)
        ));

        let (client_io, _peer_io) = tokio::io::duplex(64);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let (mut reader, writer) = WebSocketStream::client(client_io, config).split();
        drop(writer);
        assert!(reader.next().await.is_none());
    }

    #[tokio::test]
    async fn blocked_split_writer_is_cancelled_when_reader_drops() {
        let (client_io, _peer_io) = tokio::io::duplex(64);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let (reader, mut writer) = WebSocketStream::client(client_io, config).split();
        let send = tokio::spawn(async move {
            writer
                .send_binary(Bytes::from(vec![0_u8; 1024 * 1024]))
                .await
        });

        tokio::task::yield_now().await;
        drop(reader);
        assert!(matches!(send.await.unwrap(), Err(Error::ConnectionClosed)));
    }

    #[cfg(feature = "permessage-deflate")]
    #[tokio::test(start_paused = true)]
    async fn compressed_split_driver_sends_uncompressed_native_ping() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let config = Config::builder()
            .ping_interval(1)
            .pong_timeout(1)
            .idle_timeout(0)
            .build();
        let ws = CompressedWebSocketStream::client(
            client_io,
            config,
            crate::deflate::DeflateConfig::default(),
        );
        let (_reader, _writer) = ws.split();

        tokio::time::advance(Duration::from_secs(1)).await;
        let payload = read_masked_control_payload(&mut server_io, 0x09).await;
        assert_eq!(payload.len(), 8);
    }

    /// Transport wrapper that counts successful write calls.
    struct CountingIo<T> {
        inner: T,
        writes: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl<T: AsyncRead + Unpin> AsyncRead for CountingIo<T> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl<T: AsyncWrite + Unpin> AsyncWrite for CountingIo<T> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let result = Pin::new(&mut self.inner).poll_write(cx, buf);
            if matches!(result, Poll::Ready(Ok(n)) if n > 0) {
                self.writes.fetch_add(1, Ordering::Relaxed);
            }
            result
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            let result = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
            if matches!(result, Poll::Ready(Ok(n)) if n > 0) {
                self.writes.fetch_add(1, Ordering::Relaxed);
            }
            result
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// Encode `messages` as a client would (masked) into one buffer.
    fn client_frames(messages: &[Message]) -> BytesMut {
        let mut protocol = Protocol::new(Role::Client, 1 << 20, 1 << 20);
        let mut buf = BytesMut::new();
        for msg in messages {
            protocol.encode_message(msg, &mut buf).unwrap();
        }
        buf
    }

    /// Read `count` server messages from `io` with a client-side protocol.
    async fn read_server_messages<T: AsyncRead + Unpin>(io: &mut T, count: usize) -> Vec<Message> {
        let mut protocol = Protocol::new(Role::Client, 1 << 20, 1 << 20);
        let mut buf = BytesMut::with_capacity(64 * 1024);
        let mut out = Vec::new();
        while out.len() < count {
            let n = io.read_buf(&mut buf).await.unwrap();
            assert!(n > 0, "peer closed early");
            out.extend(protocol.process(&mut buf).unwrap());
        }
        out
    }

    async fn echo_batch_write_count(coalesce: bool) -> usize {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = CountingIo {
            inner: server_io,
            writes: writes.clone(),
        };
        let config = Config::builder()
            .auto_ping(false)
            .idle_timeout(0)
            .write_coalescing(coalesce)
            .build();
        let mut server = WebSocketStream::server(counting, config);

        let batch: Vec<Message> = (0..3)
            .map(|i| Message::text(format!("message {i}")))
            .collect();
        let frames = client_frames(&batch);
        let (mut client_io, server_task) = {
            let mut client_io = client_io;
            client_io.write_all(&frames).await.unwrap();
            let task = tokio::spawn(async move {
                for _ in 0..3 {
                    let msg = server.next().await.unwrap().unwrap();
                    server.send(msg).await.unwrap();
                }
                server
            });
            (client_io, task)
        };

        let echoed = read_server_messages(&mut client_io, 3).await;
        for (i, msg) in echoed.iter().enumerate() {
            assert_eq!(msg.as_text(), Some(format!("message {i}").as_str()));
        }
        let _server = server_task.await.unwrap();
        writes.load(Ordering::Relaxed)
    }

    #[tokio::test]
    async fn read_batch_answered_with_sends_is_one_write_when_coalescing() {
        // Three frames arrive in one read; three send() calls answer them.
        assert_eq!(echo_batch_write_count(true).await, 1);
        assert_eq!(echo_batch_write_count(false).await, 3);
    }

    #[tokio::test]
    async fn coalesced_frames_are_written_before_waiting_on_the_transport() {
        // The reply to the last message of a batch must not wait for more
        // inbound data: the server task blocks in next() afterwards, so the
        // client only receives the echoes if poll_next flushed first.
        let (mut client_io, server_io) = tokio::io::duplex(1 << 20);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut server = WebSocketStream::server(server_io, config);
        client_io
            .write_all(&client_frames(&[Message::text("a"), Message::text("b")]))
            .await
            .unwrap();
        let task = tokio::spawn(async move {
            for _ in 0..2 {
                let msg = server.next().await.unwrap().unwrap();
                server.send(msg).await.unwrap();
            }
            // Wait for a third message that only arrives after the client saw
            // both echoes.
            let msg = server.next().await.unwrap().unwrap();
            server.send(msg).await.unwrap();
        });
        let echoed = tokio::time::timeout(
            Duration::from_secs(2),
            read_server_messages(&mut client_io, 2),
        )
        .await
        .expect("echoes for the batch were not flushed before the next read");
        assert_eq!(echoed[0].as_text(), Some("a"));
        assert_eq!(echoed[1].as_text(), Some("b"));
        client_io
            .write_all(&client_frames(&[Message::text("c")]))
            .await
            .unwrap();
        let third = read_server_messages(&mut client_io, 1).await;
        assert_eq!(third[0].as_text(), Some("c"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn large_payloads_are_queued_by_reference_in_order() {
        let (mut client_io, server_io) = tokio::io::duplex(1 << 20);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut server = WebSocketStream::server(server_io, config);

        let big: Bytes = (0..(crate::cork::ZERO_COPY_MIN * 3 + 17))
            .map(|i| (i % 251) as u8)
            .collect::<Vec<u8>>()
            .into();
        let big_ptr = big.as_ptr();

        // Two inbound messages so both replies are coalesced into one flush:
        // header, large payload segment, then the small frame, in that order.
        client_io
            .write_all(&client_frames(&[Message::text("x"), Message::text("y")]))
            .await
            .unwrap();
        let big_for_task = big.clone();
        let task = tokio::spawn(async move {
            let _ = server.next().await.unwrap().unwrap();
            server.send(Message::Binary(big_for_task)).await.unwrap();
            let _ = server.next().await.unwrap().unwrap();
            server.send(Message::text("small")).await.unwrap();
            server
                .send(Message::Binary(Bytes::from_static(b"tail")))
                .await
                .unwrap();
        });
        let got = read_server_messages(&mut client_io, 3).await;
        assert!(matches!(&got[0], Message::Binary(b) if b == &big));
        assert_eq!(got[1].as_text(), Some("small"));
        assert!(matches!(&got[2], Message::Binary(b) if &b[..] == b"tail"));
        task.await.unwrap();
        // The payload was not copied on the sending side.
        assert_eq!(big.as_ptr(), big_ptr);
    }
}
