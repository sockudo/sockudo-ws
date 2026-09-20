//! WebSocket stream implementation
//!
//! This module provides the main `WebSocketStream` type.

use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use futures_core::Stream;
use futures_sink::Sink;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::clock::{HeartbeatTimer, Instant};
use crate::Config;
use crate::cork::CorkBuffer;
use crate::error::{CloseReason, Error, Result};
use crate::frame::{OpCode, encode_frame_header};
use crate::heartbeat::{Deadline, Heartbeat, bounded_close_reason};
use crate::protocol::{Message, Protocol, Role};

/// Maximum segments submitted in one vectored write.
const MAX_WRITE_SLICES: usize = 16;

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
        // Leftover handshake bytes must be processed once before the first read.
        has_unprocessed_read_data: bool,
        // Small frames append directly to buffer_mut(); large unmasked payloads
        // are queued as ordered segments behind their headers.
        write_buf: CorkBuffer,
        state: StreamState,
        config: Config,
        // Pending messages from last process() call
        pending_messages: Vec<Message>,
        pending_index: usize,
        // A control message is only returned after its automatic response is flushed.
        pending_control_message: Option<Message>,
        // Parse failures are returned after every complete message parsed before them.
        pending_terminal_error: Option<Error>,
        // Heartbeat failures are returned immediately after their Close frame is flushed.
        terminal_after_flush: Option<Error>,
        flush_on_read: bool,
        close_after_flush: bool,
        ping_flush_pending: bool,
        clock_epoch: Instant,
        heartbeat: Heartbeat,
        heartbeat_sleep: Option<HeartbeatTimer>,
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
        let clock_epoch = Instant::now();
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
            pending_index: 0,
            pending_control_message: None,
            pending_terminal_error: None,
            terminal_after_flush: None,
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
        if self.state != StreamState::Open || self.pending_terminal_error.is_some() {
            return Ok(());
        }

        let close = Message::Close(Some(CloseReason::new(code, reason)));
        self.protocol
            .encode_message(&close, self.write_buf.buffer_mut())?;
        self.check_write_limit()?;
        self.state = StreamState::CloseSent;

        // Flush the close frame
        self.flush_write_buf().await?;
        Ok(())
    }

    fn check_write_limit(&mut self) -> Result<()> {
        if self.write_buf.pending_bytes() > self.config.max_backpressure {
            // Rejected frames must not be sent by a later flush or close.
            self.write_buf.clear();
            self.state = StreamState::Closed;
            self.heartbeat.stop();
            self.heartbeat_sleep = None;
            return Err(Error::BufferFull);
        }
        Ok(())
    }

    /// Flush the write buffer to the underlying stream
    async fn flush_write_buf(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        while self.write_buf.has_data() {
            let n = if self.write_buf.has_segments() && self.inner.is_write_vectored() {
                let mut slices = [IoSlice::new(&[]); MAX_WRITE_SLICES];
                let count = self.write_buf.fill_write_slices(&mut slices);
                self.inner.write_vectored(&slices[..count]).await?
            } else {
                self.inner.write(self.write_buf.chunk()).await?
            };
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

        // Reclaim only an empty, uniquely owned receive window. Retained
        // message payloads and incomplete frames must keep their storage.
        if this.read_buf.is_empty() {
            let _ = this.read_buf.try_reclaim(crate::RECV_BUFFER_SIZE);
        }

        // Ensure we have space in the buffer
        if this.read_buf.capacity() - this.read_buf.len() < 4096 {
            this.read_buf.reserve(crate::RECV_BUFFER_SIZE);
        }

        // Get a slice of uninitialized memory
        let buf_len = this.read_buf.len();
        let mut read_buf = ReadBuf::uninit(this.read_buf.spare_capacity_mut());

        match this.inner.poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let n = read_buf.filled().len();
                // SAFETY: ReadBuf guarantees that its filled bytes are initialized.
                unsafe {
                    this.read_buf.set_len(buf_len + n);
                }
                if n == 0 {
                    Poll::Ready(Ok(0))
                } else {
                    Poll::Ready(Ok(n))
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Process read buffer and extract messages
    fn process_read_buf(&mut self, now_ms: Option<u64>) -> Result<()> {
        if self.read_buf.is_empty() {
            return Ok(());
        }

        let fragment_activity = self
            .protocol
            .process_into_with_activity(&mut self.read_buf, &mut self.pending_messages)?;
        if fragment_activity && let Some(now_ms) = now_ms {
            self.heartbeat.on_inbound(now_ms, None);
        }
        self.pending_index = 0;

        Ok(())
    }

    /// Send a frame, allowing read-batch coalescing when configured.
    ///
    /// Unlike `SinkExt::send`, this may return with bytes buffered while inbound
    /// messages remain queued. Call `SinkExt::flush` before pausing reads or
    /// waiting for a reply that depends on this frame. Polling for more input
    /// after the batch, or reaching the high water mark, also flushes the buffer.
    pub async fn send_coalesced(&mut self, item: Message) -> Result<()> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_ready(cx)).await?;
        Pin::new(&mut *self).start_send(item)?;
        // Batch-scoped corking: while inbound messages that were already
        // parsed are still queued for the application, keep the encoded
        // frames buffered. poll_next writes them all in one vectored write
        // before it next waits on the transport, so a read batch answered
        // with N coalesced sends costs one syscall instead of N.
        if self.config.write_coalescing
            && self.state == StreamState::Open
            && !self.pending_messages.is_empty()
            && self.write_buf.pending_bytes() < self.high_water_mark
        {
            return Ok(());
        }
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_flush(cx)).await
    }

    /// Get the next pending message (moved out, no clone)
    fn next_pending_message(&mut self) -> Option<Message> {
        if self.pending_index < self.pending_messages.len() {
            // Move the message out; the consumed slot is never returned again.
            let msg = std::mem::replace(
                &mut self.pending_messages[self.pending_index],
                Message::Close(None),
            );
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
        let tracks_activity = self.heartbeat.tracks_activity();
        let mut now_ms = None;
        loop {
            // Control responses and automatic pings must be driven by the read path.
            if self.flush_on_read {
                match self.as_mut().poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        let this = self.as_mut().get_mut();
                        this.flush_on_read = false;
                        if this.ping_flush_pending {
                            this.ping_flush_pending = false;
                            let now = this.clock_epoch.elapsed().as_millis() as u64;
                            this.heartbeat.ping_flushed(now);
                            this.heartbeat_sleep = None;
                            now_ms = Some(now);
                        }

                        if this.close_after_flush {
                            this.close_after_flush = false;
                            this.state = StreamState::Closed;
                        }

                        if let Some(msg) = this.pending_control_message.take() {
                            return Poll::Ready(Some(Ok(msg)));
                        }
                        if let Some(error) = this.terminal_after_flush.take() {
                            this.state = StreamState::Closed;
                            this.heartbeat.stop();
                            this.heartbeat_sleep = None;
                            return Poll::Ready(Some(Err(error)));
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        let this = self.as_mut().get_mut();
                        this.flush_on_read = false;
                        this.close_after_flush = false;
                        this.pending_control_message = None;
                        this.pending_terminal_error = None;
                        this.terminal_after_flush = None;
                        this.state = StreamState::Closed;
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // A peer Close is terminal even if later bytes in the same read failed parsing.
            if self.state == StreamState::Closed {
                return Poll::Ready(None);
            }

            if self.pending_messages.is_empty()
                && self.pending_control_message.is_none()
                && let Some(error) = self.as_mut().get_mut().pending_terminal_error.take()
            {
                let this = self.as_mut().get_mut();
                this.state = StreamState::Closed;
                this.heartbeat.stop();
                this.heartbeat_sleep = None;
                return Poll::Ready(Some(Err(error)));
            }

            // Heartbeat deadlines are based on inbound inactivity. A Pong
            // deadline starts only after the corresponding Ping is flushed.
            let deadline = self
                .pending_terminal_error
                .is_none()
                .then(|| self.heartbeat.next_deadline())
                .flatten();
            if now_ms.is_none() && (deadline.is_some() || tracks_activity) {
                now_ms = Some(self.clock_epoch.elapsed().as_millis() as u64);
            }
            if let Some(deadline) = deadline {
                let now = now_ms.expect("heartbeat deadline requires a clock sample");
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
                            this.terminal_after_flush = Some(error);
                            this.flush_on_read = true;
                            this.close_after_flush = true;
                        }
                    }
                    this.heartbeat_sleep = None;
                    continue;
                }
            }

            // First, return any pending messages
            if let Some(msg) = self.as_mut().get_mut().next_pending_message() {
                let this = self.as_mut().get_mut();
                if tracks_activity {
                    let pong = match &msg {
                        Message::Pong(payload) => Some(payload),
                        _ => None,
                    };
                    this.heartbeat.on_inbound(
                        now_ms.expect("activity tracking requires a clock sample"),
                        pong,
                    );
                }

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
                        this.pending_messages.clear();
                        this.pending_index = 0;
                        this.pending_terminal_error = None;
                        this.read_buf.clear();
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
                match self.as_mut().get_mut().process_read_buf(now_ms) {
                    Ok(()) if !self.pending_messages.is_empty() => continue,
                    Ok(()) => {}
                    Err(error) => {
                        let this = self.as_mut().get_mut();
                        this.pending_index = 0;
                        this.pending_terminal_error = Some(error);
                        continue;
                    }
                }
            }

            // Try to read more data
            // Write out frames coalesced from earlier sends before waiting on
            // the transport, so batch-scoped corking never delays a reply past
            // the end of the read batch.
            if self.write_buf.has_data() {
                match self.as_mut().poll_flush(cx) {
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
                Poll::Ready(Ok(_n)) => {
                    // Process the new data
                    match self.as_mut().get_mut().process_read_buf(now_ms) {
                        Ok(()) => continue, // Loop to check for messages
                        Err(error) => {
                            let this = self.as_mut().get_mut();
                            this.pending_index = 0;
                            this.pending_terminal_error = Some(error);
                            continue;
                        }
                    }
                }
                Poll::Ready(Err(e)) => {
                    let this = self.as_mut().get_mut();
                    this.state = StreamState::Closed;
                    this.heartbeat.stop();
                    this.heartbeat_sleep = None;
                    return Poll::Ready(Some(Err(e.into())));
                }
                Poll::Pending => {
                    // No more data available right now
                    if let Some(deadline) = deadline {
                        let target = self.clock_epoch + Duration::from_millis(deadline.at());
                        let sleep = self
                            .as_mut()
                            .get_mut()
                            .heartbeat_sleep
                            .get_or_insert_with(|| HeartbeatTimer::new(target));
                        if sleep.poll(target, cx).is_ready() {
                            now_ms = Some(self.clock_epoch.elapsed().as_millis() as u64);
                            continue;
                        }
                    }
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
        if self.state != StreamState::Open || self.pending_terminal_error.is_some() {
            return Poll::Ready(Err(Error::ConnectionClosed));
        }
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<()> {
        let this = self.get_mut();

        if this.state != StreamState::Open || this.pending_terminal_error.is_some() {
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
        // write picks both up in order. All paths share the byte-limit check.
        let opcode = if item.is_text() {
            OpCode::Text
        } else {
            OpCode::Binary
        };
        match item {
            Message::Text(payload) | Message::Binary(payload)
                if this.protocol.role == Role::Server
                    && payload.len() >= crate::cork::ZERO_COPY_MIN =>
            {
                encode_frame_header(
                    this.write_buf.buffer_mut(),
                    opcode,
                    payload.len(),
                    true,
                    None,
                );
                this.write_buf.push_segment(payload);
            }
            item => {
                // Encode message into write buffer
                this.protocol
                    .encode_message(&item, this.write_buf.buffer_mut())?;
            }
        }
        if this.write_buf.pending_bytes() > this.config.max_backpressure {
            // Prevent a later flush from sending frames rejected by the byte limit.
            this.write_buf.clear();
            this.state = StreamState::Closed;
            this.heartbeat.stop();
            this.heartbeat_sleep = None;
            return Err(Error::BufferFull);
        }
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.as_mut().get_mut();
        if this.state == StreamState::Closed {
            return Poll::Ready(Err(Error::ConnectionClosed));
        }

        // Write all pending data
        while this.write_buf.has_data() {
            // Keep small contiguous writes on the single-buffer path. Scatter/
            // gather only helps when a large payload was queued by reference.
            let written = if this.write_buf.has_segments() && this.inner.is_write_vectored() {
                let mut slices = [IoSlice::new(&[]); MAX_WRITE_SLICES];
                let count = this.write_buf.fill_write_slices(&mut slices);
                Pin::new(&mut this.inner).poll_write_vectored(cx, &slices[..count])
            } else {
                Pin::new(&mut this.inner).poll_write(cx, this.write_buf.chunk())
            };
            match written {
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
// Split stream implementation
// ============================================================================
//
// One background driver owns the transport writer. Both application writes and
// RFC control work use bounded queues; this is what lets Ping/Pong/Close make
// progress when the application performs zero writes.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::split_transport::SplitTransport;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

const SPLIT_CONTROL_CAPACITY: usize = 32;
const SPLIT_APPLICATION_CAPACITY: usize = 32;
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
    Ping(Bytes, Instant),
    Pong(Bytes, Instant),
    PeerClose,
    /// Start the application Close budget before it waits behind driver work.
    LocalCloseStarted(tokio::time::Instant),
    Eof,
}

#[derive(Debug)]
enum ApplicationRequest {
    Send(Message, oneshot::Sender<Result<()>>),
    Flush(oneshot::Sender<Result<()>>),
}

// Outcomes of the driver's shared deadline/application poll.
enum SplitDriverWake {
    Heartbeat,
    Application(Option<ApplicationRequest>),
}

// Outcomes of the blocked write's shared deadline/write poll.
enum SplitWriteWake {
    Deadline,
    Written(Result<()>),
}

struct SplitShared {
    /// Clock epoch shared by the reader and the writer driver
    clock_epoch: Instant,
    track_activity: bool,
    /// Milliseconds since `clock_epoch` of the last inbound data frame (reader -> driver)
    // A standalone value, not a publication barrier for other shared memory.
    /// Milliseconds since `clock_epoch` of the last inbound data frame (reader -> driver)
    last_data_ms: AtomicU64,
    status: AtomicU8,
    terminal_tx: watch::Sender<Option<TerminalCause>>,
    // One native reader per connection. Keep registration across cancelled reads.
    reader_waker: std::sync::Mutex<Option<std::task::Waker>>,
    // Writer methods require exclusive access, so at most one application
    // request can have crossed the encoder boundary.
    application_started: AtomicBool,
    cancelled: AtomicBool,
    cancel: CancellationToken,
}

impl SplitShared {
    fn new(closed: bool, config: &Config) -> Arc<Self> {
        let (terminal_tx, _) = watch::channel(closed.then_some(TerminalCause::ConnectionClosed));
        Arc::new(Self {
            clock_epoch: Instant::now(),
            track_activity: (config.auto_ping && config.ping_interval != 0)
                || config.idle_timeout != 0,
            last_data_ms: AtomicU64::new(0),
            status: AtomicU8::new(if closed { SPLIT_CLOSED } else { SPLIT_OPEN }),
            terminal_tx,
            reader_waker: std::sync::Mutex::new(None),
            application_started: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        })
    }

    fn cancel_connection(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.cancel.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
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
        self.terminal_tx.send_if_modified(|terminal| {
            if terminal.is_some() {
                return false;
            }
            *terminal = Some(cause);
            // Publish the cause before readers can observe the closed status.
            self.status.store(SPLIT_CLOSED, Ordering::Release);
            true
        });
        // Registration checks status under this same lock, so publication cannot
        // fall between the check and registration. Wake outside the lock because
        // executors may schedule a reader immediately.
        let waker = self.reader_waker.lock().unwrap().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn poll_terminal(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut registered = self.reader_waker.lock().unwrap();
        if self.status.load(Ordering::Acquire) == SPLIT_CLOSED {
            return Poll::Ready(());
        }
        if registered
            .as_ref()
            .is_none_or(|waker| !waker.will_wake(cx.waker()))
        {
            *registered = Some(cx.waker().clone());
        }
        Poll::Pending
    }

    fn is_open(&self) -> bool {
        self.status.load(Ordering::Acquire) == SPLIT_OPEN
    }

    fn record_data_activity(&self) {
        if self.track_activity {
            self.last_data_ms.store(
                self.clock_epoch.elapsed().as_millis() as u64,
                Ordering::Relaxed,
            );
        }
    }
}

trait SplitEncoder: 'static {
    fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()>;
    fn encode_pong(&mut self, payload: &[u8], buf: &mut BytesMut);
    fn encode_close_response(&mut self, buf: &mut BytesMut);
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
    reader: SplitTransport<S>,
    protocol: Protocol,
    read_buf: BytesMut,
    has_unprocessed_read_data: bool,
    pending_messages: Vec<Message>,
    pending_index: usize,
    pending_terminal_error: Option<Error>,
    control_tx: mpsc::Sender<ControlRequest>,
    terminal_rx: watch::Receiver<Option<TerminalCause>>,
    shared: Arc<SplitShared>,
    terminal_reported: bool,
}

/// The write half of a split WebSocket stream.
///
/// The transport writer itself is owned by the per-connection control driver.
/// While an open connection's write is pending, Idle/Pong expiry releases the
/// owned transport before reporting the timeout. A partial frame is never
/// followed by a Close frame; pending peer Ping replies may be coalesced.
pub struct SplitWriter<S> {
    control_tx: mpsc::Sender<ControlRequest>,
    application_tx: mpsc::Sender<ApplicationRequest>,
    shared: Arc<SplitShared>,
    _stream: PhantomData<fn() -> S>,
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
        let (reader, writer) = SplitTransport::pair(self.inner);
        let (control_tx, control_rx) = mpsc::channel(SPLIT_CONTROL_CAPACITY);
        let (application_tx, application_rx) = mpsc::channel(SPLIT_APPLICATION_CAPACITY);
        let shared = SplitShared::new(self.state != StreamState::Open, &self.config);
        // Splitting must not reopen application writes after a known parse error.
        // A preceding accepted Close still needs its automatic response.
        if self.pending_terminal_error.is_some() {
            shared.begin_closing();
            if !self.pending_messages[self.pending_index..]
                .iter()
                .any(Message::is_close)
            {
                shared.terminate(TerminalCause::ConnectionClosed);
                shared.cancel.cancel();
            }
        }
        let terminal_rx = shared.terminal_tx.subscribe();
        let writer_protocol = Protocol::new(
            self.protocol.role,
            self.config.max_frame_size,
            self.config.max_message_size,
        );
        let reader_protocol = self.protocol;

        tokio::spawn(split_writer_driver(
            writer,
            writer_protocol,
            self.config,
            control_rx,
            application_rx,
            shared.clone(),
        ));

        (
            SplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                has_unprocessed_read_data: self.has_unprocessed_read_data,
                pending_messages: self.pending_messages,
                pending_index: self.pending_index,
                pending_terminal_error: self.pending_terminal_error,
                control_tx: control_tx.clone(),
                terminal_rx,
                shared: shared.clone(),
                terminal_reported: false,
            },
            SplitWriter {
                control_tx,
                application_tx,
                shared,
                _stream: PhantomData,
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
    /// Buffered ordinary messages bypass the writer's control queue. Cooperative
    /// yields preserve them; waiting on the control queue is not cancellation-safe.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        // Buffered data no longer spends channel budget. Yield before advancing
        // the message index so cancellation preserves the next message.
        tokio::task::consume_budget().await;
        loop {
            if self.terminal_reported {
                return None;
            }
            // Stop the connection without discarding its accepted message prefix.
            // An accepted Close takes precedence over invalid bytes after it.
            if self.pending_terminal_error.is_some()
                && self.shared.is_open()
                && !self.pending_messages[self.pending_index..]
                    .iter()
                    .any(Message::is_close)
            {
                self.shared.terminate(TerminalCause::ConnectionClosed);
                self.shared.cancel.cancel();
            }
            if self.pending_terminal_error.is_none()
                && let Some(result) = self.take_terminal()
            {
                return result;
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

                if self.pending_terminal_error.is_some()
                    && self.shared.status.load(Ordering::Acquire) == SPLIT_CLOSED
                {
                    if msg.is_close() {
                        self.pending_messages.clear();
                        self.pending_index = 0;
                        self.pending_terminal_error = None;
                        self.terminal_reported = true;
                    }
                    return Some(Ok(msg));
                }
                let request = match &msg {
                    Message::Ping(data) => ControlRequest::Ping(data.clone(), Instant::now()),
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
                        self.shared.record_data_activity();
                        return Some(Ok(msg));
                    }
                };
                if self.control_tx.send(request).await.is_err() {
                    self.shared.terminate(TerminalCause::ConnectionClosed);
                    continue;
                }
                return Some(Ok(msg));
            }

            if let Some(error) = self.pending_terminal_error.take() {
                self.shared.terminate(TerminalCause::ConnectionClosed);
                self.terminal_reported = true;
                return Some(Err(error));
            }

            if self.has_unprocessed_read_data {
                self.has_unprocessed_read_data = false;
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

            // Reuse the receive window only when no unread bytes remain. If a
            // delivered message still shares it, try_reclaim leaves it alone.
            if self.read_buf.is_empty() {
                let _ = self.read_buf.try_reclaim(crate::RECV_BUFFER_SIZE);
            }

            if self.read_buf.capacity() - self.read_buf.len() < 4096 {
                self.read_buf.reserve(crate::RECV_BUFFER_SIZE);
            }

            tokio::select! {
                biased;
                result = self.reader.read_buf(&mut self.read_buf) => {
                    // A terminal cause published during the read still wins delivery.
                    if let Some(result) = self.take_terminal() {
                        return result;
                    }
                    match result {
                        Ok(0) => {
                            let _ = self.control_tx.send(ControlRequest::Eof).await;
                            self.shared.terminate(TerminalCause::ConnectionClosed);
                        }
                        Ok(_) => match self.protocol.process_into_with_activity(&mut self.read_buf, &mut self.pending_messages) {
                            Ok(fragment_activity) => {
                                if fragment_activity {
                                    self.shared.record_data_activity();
                                }
                                self.pending_index = 0;
                            }
                            Err(error) => {
                                self.pending_index = 0;
                                self.pending_terminal_error = Some(error);
                            }
                        },
                        Err(error) => {
                            self.shared.terminate(TerminalCause::ConnectionClosed);
                            return Some(Err(error.into()));
                        }
                    }
                }
                () = std::future::poll_fn(|cx| self.shared.poll_terminal(cx)) => {}
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
        // Release the task reference even when a pending next() was cancelled.
        self.shared.reader_waker.lock().unwrap().take();
        self.shared.cancel_connection();
    }
}

/// A queued request is cancellation-safe until the driver publishes that it
/// has crossed the encoder boundary. After that point the connection must stop.
struct SplitSendGuard<'a> {
    shared: &'a SplitShared,
    completed: bool,
}

impl Drop for SplitSendGuard<'_> {
    fn drop(&mut self) {
        if !self.completed && self.shared.application_started.load(Ordering::Acquire) {
            self.shared.begin_closing();
            self.shared.cancel_connection();
        }
    }
}

impl<S> SplitWriter<S> {
    /// Send a message through the connection-scoped writer driver.
    ///
    /// Cancelling before the driver starts the request drops that request and
    /// keeps the connection usable. Once encoding or transport writing starts,
    /// cancellation closes the connection unless the frame has fully flushed.
    /// An accepted Close request continues independently of its caller.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        let is_close = msg.is_close();
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        if is_close {
            let control = self
                .control_tx
                .reserve()
                .await
                .map_err(|_| self.current_error())?;
            let application = self
                .application_tx
                .reserve()
                .await
                .map_err(|_| self.current_error())?;
            if !self.shared.begin_closing() {
                return Err(self.current_error());
            }
            let (tx, rx) = oneshot::channel();
            application.send(ApplicationRequest::Send(msg, tx));
            control.send(ControlRequest::LocalCloseStarted(
                tokio::time::Instant::now(),
            ));
            return rx.await.map_err(|_| self.current_error())?;
        }

        let (tx, rx) = oneshot::channel();
        self.application_tx
            .send(ApplicationRequest::Send(msg, tx))
            .await
            .map_err(|_| self.current_error())?;
        let mut guard = SplitSendGuard {
            shared: &self.shared,
            completed: false,
        };
        let result = rx.await.map_err(|_| self.current_error());
        guard.completed = true;
        result?
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

    /// Flush all writes accepted before this request.
    pub async fn flush(&mut self) -> Result<()> {
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        let (tx, rx) = oneshot::channel();
        self.application_tx
            .send(ApplicationRequest::Flush(tx))
            .await
            .map_err(|_| self.current_error())?;
        let mut guard = SplitSendGuard {
            shared: &self.shared,
            completed: false,
        };
        let result = rx.await.map_err(|_| self.current_error());
        guard.completed = true;
        result?
    }

    /// Check whether the connection is closing or closed.
    pub fn is_closed(&self) -> bool {
        !self.shared.is_open()
    }

    fn current_error(&self) -> Error {
        self.shared
            .terminal_tx
            .borrow()
            .map_or(Error::ConnectionClosed, TerminalCause::error)
    }
}

impl<S> Drop for SplitWriter<S> {
    fn drop(&mut self) {
        self.shared.cancel_connection();
    }
}

async fn split_writer_driver<S, E>(
    mut writer: SplitTransport<S>,
    mut encoder: E,
    config: Config,
    mut control_rx: mpsc::Receiver<ControlRequest>,
    mut application_rx: mpsc::Receiver<ApplicationRequest>,
    shared: Arc<SplitShared>,
) where
    S: AsyncWrite + Unpin,
    E: SplitEncoder,
{
    let transport = writer.clone();
    let terminate = |cause| transport.close_with(|| shared.terminate(cause));
    let epoch = shared.clock_epoch;
    let mut heartbeat = Heartbeat::new(&config, epoch.elapsed().as_millis() as u64);
    let mut heartbeat_sleep = None;
    let mut closing = SplitClosing {
        deadline: None,
        timeout: Duration::from_secs(config.close_timeout.into()),
        local_close_pending: false,
    };
    let mut local_close_sent = false;
    let mut deferred_control = None;
    let mut write_buf = BytesMut::with_capacity(config.write_buffer_size);

    loop {
        // Pick up data-frame activity published by the reader without a channel.
        heartbeat.on_inbound(shared.last_data_ms.load(Ordering::Relaxed), None);
        let heartbeat_deadline = heartbeat.next_deadline();
        if heartbeat_deadline.is_none() {
            heartbeat_sleep = None;
        }
        let close_sleep = closing.deadline.map(|deadline: tokio::time::Instant| {
            tokio::time::sleep(deadline.saturating_duration_since(tokio::time::Instant::now()))
        });
        tokio::pin!(close_sleep);

        // Drain visible control events first so a timely Pong keeps its receipt time.
        // Check deadlines on every poll: a due Sleep may still await timer-wheel progress,
        // and a ready application queue must not bypass an already expired deadline.
        // Register the heartbeat Sleep only when that queue is empty; polling it on
        // every pass doubled the per-message timer cost without changing what is observed.
        tokio::select! {
            biased;
            // Dropping a half closes its sole request sender after setting this flag,
            // so channel closure wakes the driver without registering another waiter.
            _ = std::future::poll_fn(|_| {
                if shared.is_cancelled() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }) => {
                terminate(TerminalCause::ConnectionClosed);
                break;
            }
            request = async {
                match deferred_control.take() {
                    Some(request) => Some(request),
                    None => control_rx.recv().await,
                }
            } => {
                let Some(request) = request else {
                    terminate(TerminalCause::ConnectionClosed);
                    break;
                };
                match request {
                    ControlRequest::Ping(payload, received_at) => {
                        let received_ms =
                            received_at.saturating_duration_since(epoch).as_millis() as u64;
                        heartbeat.on_inbound(received_ms, None);
                        write_buf.clear();
                        encoder.encode_pong(&payload, &mut write_buf);
                        if await_split_write(
                            write_split_bytes(&mut writer, &write_buf),
                            &mut heartbeat,
                            &mut control_rx,
                            &mut deferred_control,
                            &shared,
                            &terminate,
                            &mut closing,
                        ).await.is_err() {
                            terminate(TerminalCause::ConnectionClosed);
                            break;
                        }
                    }
                    ControlRequest::Pong(payload, received_at) => {
                        let received_ms =
                            received_at.saturating_duration_since(epoch).as_millis() as u64;
                        heartbeat.on_inbound(received_ms, Some(&payload));
                    }
                    ControlRequest::LocalCloseStarted(started) => {
                        heartbeat.stop();
                        closing.begin_local(started);
                    }
                    ControlRequest::PeerClose => {
                        heartbeat.stop();
                        let deadline = closing.begin();
                        let _ = tokio::time::timeout_at(deadline, async {
                            if !local_close_sent {
                                write_buf.clear();
                                encoder.encode_close_response(&mut write_buf);
                                let _ = write_split_bytes_or_cancelled(
                                    &mut writer,
                                    &write_buf,
                                    &shared.cancel,
                                )
                                .await;
                            }
                            writer.flush().await?;
                            writer.shutdown().await
                        }).await;
                        terminate(TerminalCause::ConnectionClosed);
                        break;
                    }
                    ControlRequest::Eof => {
                        heartbeat.stop();
                        terminate(TerminalCause::ConnectionClosed);
                        break;
                    }
                }
            }
            _ = std::future::poll_fn(|cx| {
                if closing.deadline.is_some_and(|at| tokio::time::Instant::now() >= at) {
                    Poll::Ready(())
                } else {
                    close_sleep
                        .as_mut()
                        .as_pin_mut()
                        .map_or(Poll::Pending, |sleep| sleep.poll(cx))
                }
            }), if closing.deadline.is_some() && !closing.local_close_pending => {
                terminate(TerminalCause::ConnectionClosed);
                break;
            }
            wake = std::future::poll_fn(|cx| {
                if heartbeat_deadline.is_some_and(|deadline| {
                    epoch.elapsed().as_millis() as u64 >= deadline.at()
                }) {
                    return Poll::Ready(SplitDriverWake::Heartbeat);
                }
                if let Poll::Ready(request) = application_rx.poll_recv(cx) {
                    return Poll::Ready(SplitDriverWake::Application(request));
                }
                if heartbeat_deadline.is_none() {
                    return Poll::Pending;
                }
                let deadline = heartbeat_deadline.expect("deadline checked above");
                let target = epoch + Duration::from_millis(deadline.at());
                heartbeat_sleep
                    .get_or_insert_with(|| HeartbeatTimer::new(target))
                    .poll(target, cx)
                    .map(|()| SplitDriverWake::Heartbeat)
            }), if shared.is_open() || closing.local_close_pending => {
                let request = match wake {
                    SplitDriverWake::Application(request) => request,
                    SplitDriverWake::Heartbeat => {
                        // Ordinary activity can postpone this deadline without waking the driver.
                        heartbeat.on_inbound(shared.last_data_ms.load(Ordering::Relaxed), None);
                        let now_ms = epoch.elapsed().as_millis() as u64;
                        match heartbeat.next_deadline() {
                            Some(Deadline::Ping(at)) if at <= now_ms => {
                                if let Some(payload) = heartbeat.ping_due(now_ms) {
                                    write_buf.clear();
                                    if encoder
                                        .encode_message(&Message::Ping(payload), &mut write_buf)
                                        .is_err()
                                        || await_split_write(
                                            write_split_bytes(&mut writer, &write_buf),
                                            &mut heartbeat,
                                            &mut control_rx,
                                            &mut deferred_control,
                                            &shared,
                                            &terminate,
                                            &mut closing,
                                        ).await.is_err()
                                    {
                                        terminate(TerminalCause::ConnectionClosed);
                                        break;
                                    }
                                    heartbeat.ping_flushed(epoch.elapsed().as_millis() as u64);
                                }
                            }
                            Some(Deadline::Pong(at)) if at <= now_ms => {
                                timeout_close(
                                    &mut writer,
                                    &mut encoder,
                                    &config,
                                    config.pong_timeout_close_code,
                                    &config.pong_timeout_close_reason,
                                    &shared.cancel,
                                ).await;
                                terminate(TerminalCause::HeartbeatTimeout);
                                break;
                            }
                            Some(Deadline::Idle(at)) if at <= now_ms => {
                                timeout_close(
                                    &mut writer,
                                    &mut encoder,
                                    &config,
                                    CloseReason::GOING_AWAY,
                                    "Connection idle timeout",
                                    &shared.cancel,
                                ).await;
                                terminate(TerminalCause::IdleTimeout);
                                break;
                            }
                            _ => {}
                        }
                        continue;
                    }
                };
                let Some(request) = request else {
                    terminate(TerminalCause::ConnectionClosed);
                    break;
                };
                match request {
                    ApplicationRequest::Send(message, mut completion) => {
                        let is_close = message.is_close();
                        if completion.is_closed() && !is_close {
                            continue;
                        }
                        if !shared.is_open() && !(is_close && closing.local_close_pending) {
                            let _ = completion.send(Err(Error::ConnectionClosed));
                            continue;
                        }
                        if is_close {
                            closing.local_close_pending = false;
                            heartbeat.stop();
                            local_close_sent = true;
                            closing.begin();
                        } else {
                            shared.application_started.store(true, Ordering::Release);
                            if completion.is_closed() {
                                shared.application_started.store(false, Ordering::Release);
                                continue;
                            }
                        }
                        write_buf.clear();
                        let result = match encoder.encode_message(&message, &mut write_buf) {
                            Ok(()) if write_buf.len() > config.max_backpressure => Err(Error::BufferFull),
                            Ok(()) if !is_close && completion.is_closed() => {
                                Err(Error::ConnectionClosed)
                            }
                            Ok(()) if is_close => {
                                await_split_write(
                                    write_split_bytes(&mut writer, &write_buf),
                                    &mut heartbeat,
                                    &mut control_rx,
                                    &mut deferred_control,
                                    &shared,
                                    &terminate,
                                    &mut closing,
                                ).await
                            }
                            Ok(()) => tokio::select! {
                                biased;
                                result = await_split_write(
                                    write_split_bytes(&mut writer, &write_buf),
                                    &mut heartbeat,
                                    &mut control_rx,
                                    &mut deferred_control,
                                    &shared,
                                    &terminate,
                                    &mut closing,
                                ) => result,
                                _ = completion.closed() => Err(Error::ConnectionClosed),
                            },
                            Err(error) => Err(error),
                        };
                        if !is_close {
                            shared.application_started.store(false, Ordering::Release);
                        }
                        let failed = result.is_err();
                        let _ = completion.send(result);
                        if failed {
                            terminate(TerminalCause::ConnectionClosed);
                            break;
                        }
                    }
                    ApplicationRequest::Flush(mut completion) => {
                        if completion.is_closed() {
                            continue;
                        }
                        shared.application_started.store(true, Ordering::Release);
                        if completion.is_closed() {
                            shared.application_started.store(false, Ordering::Release);
                            continue;
                        }
                        let result = tokio::select! {
                            biased;
                            result = await_split_write(
                                async { writer.flush().await.map_err(Into::into) },
                                &mut heartbeat,
                                &mut control_rx,
                                &mut deferred_control,
                                &shared,
                                &terminate,
                                &mut closing,
                            ) => result,
                            _ = completion.closed() => Err(Error::ConnectionClosed),
                        };
                        shared.application_started.store(false, Ordering::Release);
                        let failed = result.is_err();
                        let _ = completion.send(result);
                        if failed {
                            terminate(TerminalCause::ConnectionClosed);
                            break;
                        }
                    }
                }
            }
        }
    }
}

// A single deadline covers the current frame, Close response and shutdown.
struct SplitClosing {
    deadline: Option<tokio::time::Instant>,
    timeout: Duration,
    // A queued local Close gets one immediate write attempt before a zero
    // deadline can win the driver's main select.
    local_close_pending: bool,
}

impl SplitClosing {
    fn begin(&mut self) -> tokio::time::Instant {
        *self
            .deadline
            .get_or_insert_with(|| tokio::time::Instant::now() + self.timeout)
    }

    fn begin_local(&mut self, started: tokio::time::Instant) {
        self.deadline.get_or_insert(started + self.timeout);
        self.local_close_pending = true;
    }
}

// Keep a single write future alive across control events: restarting write_all
// after a partial write would duplicate bytes and corrupt the current frame.
async fn await_split_write(
    write: impl Future<Output = Result<()>>,
    heartbeat: &mut Heartbeat,
    control_rx: &mut mpsc::Receiver<ControlRequest>,
    deferred_control: &mut Option<ControlRequest>,
    shared: &SplitShared,
    terminate: impl Fn(TerminalCause),
    closing: &mut SplitClosing,
) -> Result<()> {
    tokio::pin!(write);
    let immediate = std::future::poll_fn(|cx| {
        Poll::Ready(match write.as_mut().poll(cx) {
            Poll::Ready(result) => Some(result),
            Poll::Pending => None,
        })
    })
    .await;
    if let Some(result) = immediate {
        return result;
    }
    // Keep the registration across control events while the logical deadline is
    // unchanged. The pinned option owns Sleep in this future, without a Box.
    let sleep: Option<tokio::time::Sleep> = None;
    tokio::pin!(sleep);
    let mut armed_deadline = None;
    loop {
        // Pick up data-frame activity published by the reader without a channel.
        heartbeat.on_inbound(shared.last_data_ms.load(Ordering::Relaxed), None);
        let deadline = heartbeat.next_timeout();
        let close_at = closing.deadline;
        if Some((deadline, close_at)) != armed_deadline {
            let runtime_deadline = deadline.map(|at| {
                // Sample the runtime first; an early wake is checked against the
                // logical clock below, rather than treating timer readiness as expiry.
                let runtime_now = tokio::time::Instant::now();
                let delay = Duration::from_millis(
                    at.at()
                        .saturating_sub(shared.clock_epoch.elapsed().as_millis() as u64),
                );
                runtime_now + delay
            });
            let runtime_deadline = match (runtime_deadline, close_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            if let Some(runtime_deadline) = runtime_deadline {
                if let Some(mut timer) = sleep.as_mut().as_pin_mut() {
                    timer.as_mut().reset(runtime_deadline);
                } else {
                    sleep.set(Some(tokio::time::sleep_until(runtime_deadline)));
                }
            } else {
                sleep.set(None);
            }
            armed_deadline = Some((deadline, close_at));
        }
        tokio::select! {
            biased;
            _ = shared.cancel.cancelled() => return Err(Error::ConnectionClosed),
            request = control_rx.recv(), if !matches!(deferred_control, Some(ControlRequest::PeerClose)) => {
                match request {
                    Some(ControlRequest::Pong(payload, received_at)) => {
                        let received_ms = received_at.saturating_duration_since(shared.clock_epoch).as_millis() as u64;
                        heartbeat.on_inbound(received_ms, Some(&payload));
                    }
                    Some(ControlRequest::Ping(payload, received_at)) => {
                        let received_ms = received_at.saturating_duration_since(shared.clock_epoch).as_millis() as u64;
                        heartbeat.on_inbound(received_ms, None);
                        // RFC 6455 §5.5.3 permits replying only to the latest Ping
                        // when previous Pongs have not yet been sent. Keep this bounded
                        // while allowing a matching Pong behind peer Pings to progress.
                        *deferred_control = Some(ControlRequest::Ping(payload, received_at));
                    }
                    Some(ControlRequest::PeerClose) => {
                        // Finish the current frame within the same closing budget
                        // as the response; never append Close to a partial frame.
                        heartbeat.stop();
                        closing.begin();
                        *deferred_control = Some(ControlRequest::PeerClose);
                    }
                    Some(ControlRequest::LocalCloseStarted(started)) => {
                        heartbeat.stop();
                        closing.begin_local(started);
                    }
                    Some(ControlRequest::Eof) | None => return Err(Error::ConnectionClosed),
                }
            }
            wake = std::future::poll_fn(|cx| {
                if close_at.is_some_and(|at| tokio::time::Instant::now() >= at)
                    || deadline.is_some_and(|at| shared.clock_epoch.elapsed().as_millis() as u64 >= at.at()) {
                    return Poll::Ready(SplitWriteWake::Deadline);
                }
                if let Poll::Ready(result) = write.as_mut().poll(cx) {
                    return Poll::Ready(SplitWriteWake::Written(result));
                }
                if deadline.is_none() && close_at.is_none() {
                    return Poll::Pending;
                }
                sleep.as_mut().as_pin_mut().unwrap().poll(cx).map(|()| SplitWriteWake::Deadline)
            }) => match wake {
                SplitWriteWake::Deadline => {
                    if close_at.is_some_and(|at| tokio::time::Instant::now() >= at) {
                        terminate(TerminalCause::ConnectionClosed);
                        return Err(Error::ConnectionClosed);
                    }
                    heartbeat.on_inbound(shared.last_data_ms.load(Ordering::Relaxed), None);
                    let now_ms = shared.clock_epoch.elapsed().as_millis() as u64;
                    let cause = match heartbeat.next_timeout() {
                        Some(Deadline::Pong(at)) if at <= now_ms => TerminalCause::HeartbeatTimeout,
                        Some(Deadline::Idle(at)) if at <= now_ms => TerminalCause::IdleTimeout,
                        _ => {
                            // A runtime wake may precede the logical deadline.
                            // Rebase before polling again instead of spinning on Ready.
                            armed_deadline = None;
                            continue;
                        },
                    };
                    // Release the transport before publishing the cause or completing
                    // requests. Dropping only the writer would leave the read handle's
                    // reference alive and could retain a blocked TCP/TLS connection.
                    terminate(cause);
                    return Err(cause.error());
                }
                SplitWriteWake::Written(result) => return result,
            }
        }
    }
}

async fn write_split_bytes<S>(writer: &mut SplitTransport<S>, bytes: &[u8]) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    writer.write_all_and_flush(bytes).await?;
    Ok(())
}

async fn write_split_bytes_or_cancelled<S>(
    writer: &mut SplitTransport<S>,
    bytes: &[u8],
    cancel: &CancellationToken,
) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    tokio::select! {
        result = write_split_bytes(writer, bytes) => result,
        _ = cancel.cancelled() => Err(Error::ConnectionClosed),
    }
}

async fn timeout_close<S, E>(
    writer: &mut SplitTransport<S>,
    encoder: &mut E,
    config: &Config,
    code: u16,
    reason: &str,
    cancel: &CancellationToken,
) where
    S: AsyncWrite + Unpin,
    E: SplitEncoder,
{
    let _ = tokio::time::timeout(Duration::from_secs(config.close_timeout.into()), async {
        let mut buf = BytesMut::with_capacity(128);
        let close = Message::Close(Some(CloseReason::new(code, bounded_close_reason(reason))));
        if encoder.encode_message(&close, &mut buf).is_ok() {
            let _ = write_split_bytes_or_cancelled(writer, &buf, cancel).await;
        }
        writer.flush().await?;
        writer.shutdown().await
    })
    .await;
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
        has_unprocessed_read_data: bool,
        // Compressed frame encoders append directly to buffer_mut(), so flushes
        // have one contiguous slice without queued segments.
        write_buf: CorkBuffer,
        state: StreamState,
        config: Config,
        pending_messages: Vec<Message>,
        pending_index: usize,
        pending_control_message: Option<Message>,
        // Parse failures are returned after every complete message parsed before them.
        pending_terminal_error: Option<Error>,
        // Heartbeat failures are returned immediately after their Close frame is flushed.
        terminal_after_flush: Option<Error>,
        flush_on_read: bool,
        close_after_flush: bool,
        ping_flush_pending: bool,
        clock_epoch: Instant,
        heartbeat: Heartbeat,
        heartbeat_sleep: Option<HeartbeatTimer>,
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
        Self::server_with_leftover(inner, config, deflate_config, None)
    }

    /// Create a server-side compressed stream with post-handshake leftover bytes.
    pub fn server_with_leftover(
        inner: S,
        config: Config,
        deflate_config: crate::deflate::DeflateConfig,
        leftover: Option<Bytes>,
    ) -> Self {
        let protocol = if config.compression.is_shared() {
            crate::protocol::CompressedProtocol::server_with_shared_compression(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            )
        } else {
            crate::protocol::CompressedProtocol::server(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            )
        };

        let mut read_buf = BytesMut::with_capacity(crate::RECV_BUFFER_SIZE);
        if let Some(leftover) = leftover {
            read_buf.extend_from_slice(&leftover);
        }
        let has_unprocessed_read_data = !read_buf.is_empty();

        let clock_epoch = Instant::now();
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
            pending_index: 0,
            pending_control_message: None,
            pending_terminal_error: None,
            terminal_after_flush: None,
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
        Self::client_with_leftover(inner, config, deflate_config, None)
    }

    /// Create a client-side compressed stream with post-handshake leftover bytes.
    pub fn client_with_leftover(
        inner: S,
        config: Config,
        deflate_config: crate::deflate::DeflateConfig,
        leftover: Option<Bytes>,
    ) -> Self {
        let protocol = if config.compression.is_shared() {
            crate::protocol::CompressedProtocol::client_with_shared_compression(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            )
        } else {
            crate::protocol::CompressedProtocol::client(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            )
        };

        let mut read_buf = BytesMut::with_capacity(crate::RECV_BUFFER_SIZE);
        if let Some(leftover) = leftover {
            read_buf.extend_from_slice(&leftover);
        }
        let has_unprocessed_read_data = !read_buf.is_empty();

        let clock_epoch = Instant::now();
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
            pending_index: 0,
            pending_control_message: None,
            pending_terminal_error: None,
            terminal_after_flush: None,
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
        if self.state != StreamState::Open || self.pending_terminal_error.is_some() {
            return Ok(());
        }

        let close = Message::Close(Some(CloseReason::new(code, reason)));
        self.protocol
            .encode_message(&close, self.write_buf.buffer_mut())?;
        self.check_write_limit()?;
        self.state = StreamState::CloseSent;

        self.flush_write_buf().await?;
        Ok(())
    }

    fn check_write_limit(&mut self) -> Result<()> {
        if self.write_buf.pending_bytes() > self.config.max_backpressure {
            // Rejected frames must not be sent by a later flush or close.
            self.write_buf.clear();
            self.state = StreamState::Closed;
            self.heartbeat.stop();
            self.heartbeat_sleep = None;
            return Err(Error::BufferFull);
        }
        Ok(())
    }

    /// Flush the write buffer to the underlying stream
    async fn flush_write_buf(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        while self.write_buf.has_data() {
            let n = if self.write_buf.has_segments() && self.inner.is_write_vectored() {
                let mut slices = [IoSlice::new(&[]); MAX_WRITE_SLICES];
                let count = self.write_buf.fill_write_slices(&mut slices);
                self.inner.write_vectored(&slices[..count]).await?
            } else {
                self.inner.write(self.write_buf.chunk()).await?
            };
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

        // Reclaim only an empty, uniquely owned receive window. Retained
        // message payloads and incomplete frames must keep their storage.
        if this.read_buf.is_empty() {
            let _ = this.read_buf.try_reclaim(crate::RECV_BUFFER_SIZE);
        }

        if this.read_buf.capacity() - this.read_buf.len() < 4096 {
            this.read_buf.reserve(crate::RECV_BUFFER_SIZE);
        }

        let buf_len = this.read_buf.len();
        let mut read_buf = ReadBuf::uninit(this.read_buf.spare_capacity_mut());

        match this.inner.poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let n = read_buf.filled().len();
                // SAFETY: ReadBuf guarantees that its filled bytes are initialized.
                unsafe {
                    this.read_buf.set_len(buf_len + n);
                }
                if n == 0 {
                    Poll::Ready(Ok(0))
                } else {
                    Poll::Ready(Ok(n))
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Process read buffer and extract messages
    fn process_read_buf(&mut self, now_ms: Option<u64>) -> Result<()> {
        if self.read_buf.is_empty() {
            return Ok(());
        }

        let fragment_activity = self
            .protocol
            .process_into_with_activity(&mut self.read_buf, &mut self.pending_messages)?;
        if fragment_activity && let Some(now_ms) = now_ms {
            self.heartbeat.on_inbound(now_ms, None);
        }
        self.pending_index = 0;

        Ok(())
    }

    /// Send a frame, allowing read-batch coalescing when configured.
    ///
    /// Unlike `SinkExt::send`, this may return with bytes buffered while inbound
    /// messages remain queued. Call `SinkExt::flush` before pausing reads or
    /// waiting for a reply that depends on this frame. Polling for more input
    /// after the batch, or reaching the high water mark, also flushes the buffer.
    pub async fn send_coalesced(&mut self, item: Message) -> Result<()> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_ready(cx)).await?;
        Pin::new(&mut *self).start_send(item)?;
        // Batch-scoped corking: while inbound messages that were already
        // parsed are still queued for the application, keep the encoded
        // frames buffered. poll_next writes them all in one vectored write
        // before it next waits on the transport, so a read batch answered
        // with N coalesced sends costs one syscall instead of N.
        if self.config.write_coalescing
            && self.state == StreamState::Open
            && !self.pending_messages.is_empty()
            && self.write_buf.pending_bytes() < self.high_water_mark
        {
            return Ok(());
        }
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_flush(cx)).await
    }

    /// Get the next pending message (moved out, no clone)
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
}

#[cfg(feature = "permessage-deflate")]
impl<S> Stream for CompressedWebSocketStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Item = Result<Message>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let tracks_activity = self.heartbeat.tracks_activity();
        let mut now_ms = None;
        loop {
            if self.flush_on_read {
                match self.as_mut().poll_flush(cx) {
                    Poll::Ready(Ok(())) => {
                        let this = self.as_mut().get_mut();
                        this.flush_on_read = false;
                        if this.ping_flush_pending {
                            this.ping_flush_pending = false;
                            let now = this.clock_epoch.elapsed().as_millis() as u64;
                            this.heartbeat.ping_flushed(now);
                            this.heartbeat_sleep = None;
                            now_ms = Some(now);
                        }

                        if this.close_after_flush {
                            this.close_after_flush = false;
                            this.state = StreamState::Closed;
                        }

                        if let Some(msg) = this.pending_control_message.take() {
                            return Poll::Ready(Some(Ok(msg)));
                        }
                        if let Some(error) = this.terminal_after_flush.take() {
                            this.state = StreamState::Closed;
                            this.heartbeat.stop();
                            this.heartbeat_sleep = None;
                            return Poll::Ready(Some(Err(error)));
                        }
                    }
                    Poll::Ready(Err(e)) => {
                        let this = self.as_mut().get_mut();
                        this.flush_on_read = false;
                        this.close_after_flush = false;
                        this.pending_control_message = None;
                        this.pending_terminal_error = None;
                        this.terminal_after_flush = None;
                        this.state = StreamState::Closed;
                        this.heartbeat.stop();
                        this.heartbeat_sleep = None;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // A peer Close is terminal even if later bytes in the same read failed parsing.
            if self.state == StreamState::Closed {
                return Poll::Ready(None);
            }

            if self.pending_messages.is_empty()
                && self.pending_control_message.is_none()
                && let Some(error) = self.as_mut().get_mut().pending_terminal_error.take()
            {
                let this = self.as_mut().get_mut();
                this.state = StreamState::Closed;
                this.heartbeat.stop();
                this.heartbeat_sleep = None;
                return Poll::Ready(Some(Err(error)));
            }

            let deadline = self
                .pending_terminal_error
                .is_none()
                .then(|| self.heartbeat.next_deadline())
                .flatten();
            if now_ms.is_none() && (deadline.is_some() || tracks_activity) {
                now_ms = Some(self.clock_epoch.elapsed().as_millis() as u64);
            }
            if let Some(deadline) = deadline {
                let now = now_ms.expect("heartbeat deadline requires a clock sample");
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
                            this.terminal_after_flush = Some(error);
                            this.flush_on_read = true;
                            this.close_after_flush = true;
                        }
                    }
                    this.heartbeat_sleep = None;
                    continue;
                }
            }

            if let Some(msg) = self.as_mut().get_mut().next_pending_message() {
                let this = self.as_mut().get_mut();
                if tracks_activity {
                    let pong = match &msg {
                        Message::Pong(payload) => Some(payload),
                        _ => None,
                    };
                    this.heartbeat.on_inbound(
                        now_ms.expect("activity tracking requires a clock sample"),
                        pong,
                    );
                }

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
                        this.pending_messages.clear();
                        this.pending_index = 0;
                        this.pending_terminal_error = None;
                        this.read_buf.clear();
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

            if self.has_unprocessed_read_data {
                self.as_mut().get_mut().has_unprocessed_read_data = false;
                match self.as_mut().get_mut().process_read_buf(now_ms) {
                    Ok(()) if !self.pending_messages.is_empty() => continue,
                    Ok(()) => {}
                    Err(error) => {
                        let this = self.as_mut().get_mut();
                        this.pending_index = 0;
                        this.pending_terminal_error = Some(error);
                        continue;
                    }
                }
            }

            // Write out frames coalesced from earlier sends before waiting on
            // the transport, so batch-scoped corking never delays a reply past
            // the end of the read batch.
            if self.write_buf.has_data() {
                match self.as_mut().poll_flush(cx) {
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
                Poll::Ready(Ok(_n)) => match self.as_mut().get_mut().process_read_buf(now_ms) {
                    Ok(()) => continue,
                    Err(error) => {
                        let this = self.as_mut().get_mut();
                        this.pending_index = 0;
                        this.pending_terminal_error = Some(error);
                        continue;
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
                    if let Some(deadline) = deadline {
                        let target = self.clock_epoch + Duration::from_millis(deadline.at());
                        let sleep = self
                            .as_mut()
                            .get_mut()
                            .heartbeat_sleep
                            .get_or_insert_with(|| HeartbeatTimer::new(target));
                        if sleep.poll(target, cx).is_ready() {
                            now_ms = Some(self.clock_epoch.elapsed().as_millis() as u64);
                            continue;
                        }
                    }
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
        if self.state != StreamState::Open || self.pending_terminal_error.is_some() {
            return Poll::Ready(Err(Error::ConnectionClosed));
        }
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<()> {
        let this = self.get_mut();

        if this.state != StreamState::Open || this.pending_terminal_error.is_some() {
            return Err(Error::ConnectionClosed);
        }

        if item.is_close() {
            this.state = StreamState::CloseSent;
            this.heartbeat.stop();
            this.heartbeat_sleep = None;
        }

        this.protocol
            .encode_message(&item, this.write_buf.buffer_mut())?;
        if this.write_buf.pending_bytes() > this.config.max_backpressure {
            // Compression may already have advanced its dictionary. Terminate
            // rather than discard this frame and continue with divergent state.
            this.write_buf.clear();
            this.state = StreamState::Closed;
            this.heartbeat.stop();
            this.heartbeat_sleep = None;
            return Err(Error::BufferFull);
        }
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let this = self.as_mut().get_mut();
        if this.state == StreamState::Closed {
            return Poll::Ready(Err(Error::ConnectionClosed));
        }

        while this.write_buf.has_data() {
            match Pin::new(&mut this.inner).poll_write(cx, this.write_buf.chunk()) {
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
/// This half owns the transport reader and decoder. Ordinary messages bypass
/// the writer's control queue; control events still use the connection driver.
#[cfg(feature = "permessage-deflate")]
pub struct CompressedSplitReader<S> {
    /// Read half of the underlying stream
    reader: SplitTransport<S>,
    /// Protocol for decoding with decompression
    protocol: crate::protocol::CompressedReaderProtocol,
    /// Read buffer
    read_buf: BytesMut,
    has_unprocessed_read_data: bool,
    /// Pending messages from last decode
    pending_messages: Vec<Message>,
    pending_index: usize,
    pending_terminal_error: Option<Error>,
    control_tx: mpsc::Sender<ControlRequest>,
    terminal_rx: watch::Receiver<Option<TerminalCause>>,
    shared: Arc<SplitShared>,
    terminal_reported: bool,
}

/// The write half of a split compressed WebSocket stream
///
/// Created by calling `split()` on a `CompressedWebSocketStream`.
/// This handle submits writes to the connection driver, which owns the transport
/// writer and encoder. The reader maintains its decoder separately.
/// Blocked-write timeouts and transport release follow [`SplitWriter`].
#[cfg(feature = "permessage-deflate")]
pub struct CompressedSplitWriter<S> {
    control_tx: mpsc::Sender<ControlRequest>,
    application_tx: mpsc::Sender<ApplicationRequest>,
    shared: Arc<SplitShared>,
    _stream: PhantomData<fn() -> S>,
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
    /// Reading and writing can run in different tasks. A private mutex synchronizes
    /// transport polls and lets the driver release a timed-out stream even while
    /// the read handle remains alive. Ordinary messages bypass
    /// the writer's control queue, while protocol controls use its driver.
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
    /// // Read messages in one task
    /// tokio::spawn(async move {
    ///     while let Some(msg) = reader.next().await {
    ///         println!("Got: {:?}", msg);
    ///     }
    /// });
    ///
    /// // Submit an application write in another task
    /// writer.send(Message::Text("Hello".into())).await?;
    /// ```
    pub fn split(self) -> (CompressedSplitReader<S>, CompressedSplitWriter<S>) {
        // Share transport access with a driver that can release it on timeout
        let (reader, writer) = SplitTransport::pair(self.inner);

        let (control_tx, control_rx) = mpsc::channel(SPLIT_CONTROL_CAPACITY);
        let (application_tx, application_rx) = mpsc::channel(SPLIT_APPLICATION_CAPACITY);
        let shared = SplitShared::new(self.state != StreamState::Open, &self.config);
        // Splitting must not reopen application writes after a known parse error.
        // A preceding accepted Close still needs its automatic response.
        if self.pending_terminal_error.is_some() {
            shared.begin_closing();
            if !self.pending_messages[self.pending_index..]
                .iter()
                .any(Message::is_close)
            {
                shared.terminate(TerminalCause::ConnectionClosed);
                shared.cancel.cancel();
            }
        }
        let terminal_rx = shared.terminal_tx.subscribe();

        // Split the protocol into reader and writer halves
        let (reader_protocol, writer_protocol) = self
            .protocol
            .split(self.config.max_frame_size, self.config.max_message_size);

        tokio::spawn(split_writer_driver(
            writer,
            writer_protocol,
            self.config,
            control_rx,
            application_rx,
            shared.clone(),
        ));

        (
            CompressedSplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                has_unprocessed_read_data: self.has_unprocessed_read_data,
                pending_messages: self.pending_messages,
                pending_index: self.pending_index,
                pending_terminal_error: self.pending_terminal_error,
                control_tx: control_tx.clone(),
                terminal_rx,
                shared: shared.clone(),
                terminal_reported: false,
            },
            CompressedSplitWriter {
                control_tx,
                application_tx,
                shared,
                _stream: PhantomData,
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
    /// Buffered ordinary messages bypass the writer's control queue. Cooperative
    /// yields preserve them; waiting on the control queue is not cancellation-safe.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        // Buffered data no longer spends channel budget. Yield before advancing
        // the message index so cancellation preserves the next message.
        tokio::task::consume_budget().await;
        loop {
            if self.terminal_reported {
                return None;
            }
            // Stop the connection without discarding its accepted message prefix.
            // An accepted Close takes precedence over invalid bytes after it.
            if self.pending_terminal_error.is_some()
                && self.shared.is_open()
                && !self.pending_messages[self.pending_index..]
                    .iter()
                    .any(Message::is_close)
            {
                self.shared.terminate(TerminalCause::ConnectionClosed);
                self.shared.cancel.cancel();
            }
            if self.pending_terminal_error.is_none()
                && let Some(result) = self.take_terminal()
            {
                return result;
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

                if self.pending_terminal_error.is_some()
                    && self.shared.status.load(Ordering::Acquire) == SPLIT_CLOSED
                {
                    if msg.is_close() {
                        self.pending_messages.clear();
                        self.pending_index = 0;
                        self.pending_terminal_error = None;
                        self.terminal_reported = true;
                    }
                    return Some(Ok(msg));
                }
                let request = match &msg {
                    Message::Ping(data) => ControlRequest::Ping(data.clone(), Instant::now()),
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
                        self.shared.record_data_activity();
                        return Some(Ok(msg));
                    }
                };
                if self.control_tx.send(request).await.is_err() {
                    self.shared.terminate(TerminalCause::ConnectionClosed);
                    continue;
                }
                return Some(Ok(msg));
            }

            if let Some(error) = self.pending_terminal_error.take() {
                self.shared.terminate(TerminalCause::ConnectionClosed);
                self.terminal_reported = true;
                return Some(Err(error));
            }

            if self.has_unprocessed_read_data {
                self.has_unprocessed_read_data = false;
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

            // As in the uncompressed reader, retained payloads prevent reclaim.
            if self.read_buf.is_empty() {
                let _ = self.read_buf.try_reclaim(crate::RECV_BUFFER_SIZE);
            }

            if self.read_buf.capacity() - self.read_buf.len() < 4096 {
                self.read_buf.reserve(crate::RECV_BUFFER_SIZE);
            }

            tokio::select! {
                biased;
                result = self.reader.read_buf(&mut self.read_buf) => {
                    // A terminal cause published during the read still wins delivery.
                    if let Some(result) = self.take_terminal() {
                        return result;
                    }
                    match result {
                        Ok(0) => {
                            let _ = self.control_tx.send(ControlRequest::Eof).await;
                            self.shared.terminate(TerminalCause::ConnectionClosed);
                        }
                        Ok(_) => match self.protocol.process_into_with_activity(&mut self.read_buf, &mut self.pending_messages) {
                            Ok(fragment_activity) => {
                                if fragment_activity {
                                    self.shared.record_data_activity();
                                }
                                self.pending_index = 0;
                            }
                            Err(error) => {
                                self.pending_index = 0;
                                self.pending_terminal_error = Some(error);
                            }
                        },
                        Err(error) => {
                            self.shared.terminate(TerminalCause::ConnectionClosed);
                            return Some(Err(error.into()));
                        }
                    }
                }
                () = std::future::poll_fn(|cx| self.shared.poll_terminal(cx)) => {}
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
        // Release the task reference even when a pending next() was cancelled.
        self.shared.reader_waker.lock().unwrap().take();
        self.shared.cancel_connection();
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> CompressedSplitWriter<S> {
    /// Send a message through the connection-scoped writer driver.
    ///
    /// Cancelling before the driver starts the request drops that request and
    /// keeps the connection usable. Once encoding or transport writing starts,
    /// cancellation closes the connection unless the frame has fully flushed.
    /// An accepted Close request continues independently of its caller.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        let is_close = msg.is_close();
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        if is_close {
            let control = self
                .control_tx
                .reserve()
                .await
                .map_err(|_| self.current_error())?;
            let application = self
                .application_tx
                .reserve()
                .await
                .map_err(|_| self.current_error())?;
            if !self.shared.begin_closing() {
                return Err(self.current_error());
            }
            let (tx, rx) = oneshot::channel();
            application.send(ApplicationRequest::Send(msg, tx));
            control.send(ControlRequest::LocalCloseStarted(
                tokio::time::Instant::now(),
            ));
            return rx.await.map_err(|_| self.current_error())?;
        }

        let (tx, rx) = oneshot::channel();
        self.application_tx
            .send(ApplicationRequest::Send(msg, tx))
            .await
            .map_err(|_| self.current_error())?;
        let mut guard = SplitSendGuard {
            shared: &self.shared,
            completed: false,
        };
        let result = rx.await.map_err(|_| self.current_error());
        guard.completed = true;
        result?
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

    pub fn is_closed(&self) -> bool {
        !self.shared.is_open()
    }

    pub async fn flush(&mut self) -> Result<()> {
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        let (tx, rx) = oneshot::channel();
        self.application_tx
            .send(ApplicationRequest::Flush(tx))
            .await
            .map_err(|_| self.current_error())?;
        let mut guard = SplitSendGuard {
            shared: &self.shared,
            completed: false,
        };
        let result = rx.await.map_err(|_| self.current_error());
        guard.completed = true;
        result?
    }

    fn current_error(&self) -> Error {
        self.shared
            .terminal_tx
            .borrow()
            .map_or(Error::ConnectionClosed, TerminalCause::error)
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> Drop for CompressedSplitWriter<S> {
    fn drop(&mut self) {
        self.shared.cancel_connection();
    }
}

#[cfg(test)]
mod receive_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{StreamExt, poll};
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

    #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
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

    #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
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

    #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
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

    #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
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
        let config = Config::builder()
            .auto_ping(false)
            .idle_timeout(0)
            .max_backpressure(2 * 1024 * 1024)
            .build();
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
    #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
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
                    server.send_coalesced(msg).await.unwrap();
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
        // Three frames arrive in one read; three send_coalesced() calls answer them.
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
                server.send_coalesced(msg).await.unwrap();
            }
            // Wait for a third message that only arrives after the client saw
            // both echoes.
            let msg = server.next().await.unwrap().unwrap();
            server.send_coalesced(msg).await.unwrap();
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
            server
                .send_coalesced(Message::Binary(big_for_task))
                .await
                .unwrap();
            let _ = server.next().await.unwrap().unwrap();
            server.send_coalesced(Message::text("small")).await.unwrap();
            server
                .send_coalesced(Message::Binary(Bytes::from_static(b"tail")))
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

    struct DeadlineCrossingEncoder {
        first_encode_delay: Duration,
    }

    impl SplitEncoder for DeadlineCrossingEncoder {
        fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()> {
            if !matches!(msg, Message::Close(_)) {
                // Model synchronous compression that crosses a deadline without yielding.
                std::thread::sleep(self.first_encode_delay);
                buf.extend_from_slice(b"\x82\x01x");
            } else {
                buf.extend_from_slice(b"\x88\x00");
            }
            Ok(())
        }

        fn encode_pong(&mut self, _payload: &[u8], buf: &mut BytesMut) {
            buf.extend_from_slice(b"\x8a\x00");
        }

        fn encode_close_response(&mut self, buf: &mut BytesMut) {
            buf.extend_from_slice(b"\x88\x00");
        }
    }

    struct TerminalRead {
        shared: Arc<std::sync::Mutex<Option<Arc<SplitShared>>>>,
    }

    impl AsyncRead for TerminalRead {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            // Model a concurrent terminal publication after the reader's
            // initial status check but before its I/O result is delivered.
            self.shared
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .terminate(TerminalCause::IdleTimeout);
            Poll::Ready(Err(std::io::ErrorKind::ConnectionReset.into()))
        }
    }

    impl AsyncWrite for TerminalRead {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(data.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn split_read_preserves_terminal_cause_published_during_io() {
        let shared = Arc::new(std::sync::Mutex::new(None));
        let (mut reader, _writer) = WebSocketStream::client(
            TerminalRead {
                shared: shared.clone(),
            },
            Config::default(),
        )
        .split();
        *shared.lock().unwrap() = Some(reader.shared.clone());

        assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
        assert!(reader.next().await.is_none());
    }

    #[cfg(feature = "permessage-deflate")]
    #[tokio::test]
    async fn compressed_split_read_preserves_terminal_cause_published_during_io() {
        let shared = Arc::new(std::sync::Mutex::new(None));
        let (mut reader, _writer) = CompressedWebSocketStream::client(
            TerminalRead {
                shared: shared.clone(),
            },
            Config::default(),
            crate::receive_tests::deflate_config(),
        )
        .split();
        *shared.lock().unwrap() = Some(reader.shared.clone());

        assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
        assert!(reader.next().await.is_none());
    }

    #[test]
    fn accepted_write_finishes_when_idle_expires_during_encoding() {
        crate::init_clock();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (io, mut peer) = tokio::io::duplex(1024);
                let (_reader, writer) = SplitTransport::pair(io);
                let config = Config::builder().auto_ping(false).idle_timeout(1).build();
                let (control_tx, control_rx) = mpsc::channel(SPLIT_CONTROL_CAPACITY);
                let (application_tx, application_rx) = mpsc::channel(SPLIT_APPLICATION_CAPACITY);
                let shared = SplitShared::new(false, &config);
                let mut terminal_rx = shared.terminal_tx.subscribe();
                let (completion_tx, completion_rx) = oneshot::channel();

                application_tx
                    .send(ApplicationRequest::Send(
                        Message::text("accepted"),
                        completion_tx,
                    ))
                    .await
                    .unwrap();
                let driver = tokio::spawn(split_writer_driver(
                    writer,
                    DeadlineCrossingEncoder {
                        first_encode_delay: Duration::from_millis(1100),
                    },
                    config,
                    control_rx,
                    application_rx,
                    shared,
                ));

                assert!(completion_rx.await.unwrap().is_ok());
                let mut frames = [0; 5];
                peer.read_exact(&mut frames).await.unwrap();
                assert_eq!(frames, [0x82, 0x01, b'x', 0x88, 0x00]);
                terminal_rx.changed().await.unwrap();
                assert_eq!(*terminal_rx.borrow(), Some(TerminalCause::IdleTimeout));

                driver.await.unwrap();
                drop(control_tx);
                drop(application_tx);
            });
    }

    #[tokio::test]
    async fn split_read_keeps_retained_payload_valid_across_receive_windows() {
        let (io, mut peer) = tokio::io::duplex(4096);
        let producer = tokio::spawn(async move {
            for sequence in 0..1024_u16 {
                let mut frame = [0; 260];
                frame[..4].copy_from_slice(&[0x82, 126, 1, 0]);
                frame[4..].fill((sequence % 251) as u8);
                peer.write_all(&frame).await.unwrap();
            }
        });
        let (mut reader, _writer) = WebSocketStream::client(io, Config::default()).split();
        let retained = reader.next().await.unwrap().unwrap();
        for sequence in 1..1024_u16 {
            let message = reader.next().await.unwrap().unwrap();
            assert_eq!(message.as_bytes(), &[(sequence % 251) as u8; 256]);
        }
        producer.await.unwrap();
        assert_eq!(retained.as_bytes(), &[0; 256]);
    }

    #[tokio::test]
    async fn ready_message_does_not_register_heartbeat_timer() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let mut ws = WebSocketStream::client(client_io, Config::default());
        server_io.write_all(b"\x82\x01x").await.unwrap();

        assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"x");
        assert!(ws.heartbeat_sleep.is_none());
    }

    #[cfg(feature = "permessage-deflate")]
    #[tokio::test]
    async fn compressed_ready_message_does_not_register_heartbeat_timer() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let mut ws = CompressedWebSocketStream::client(
            client_io,
            Config::default(),
            crate::deflate::DeflateConfig::default(),
        );
        server_io.write_all(b"\x82\x01x").await.unwrap();

        assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"x");
        assert!(ws.heartbeat_sleep.is_none());
    }

    #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
    #[tokio::test(start_paused = true)]
    async fn heartbeat_timer_is_registered_only_before_pending() {
        let configs = [
            ("default", Config::default(), true),
            (
                "idle only",
                Config::builder().auto_ping(false).idle_timeout(1).build(),
                true,
            ),
            (
                "ping only",
                Config::builder().ping_interval(1).idle_timeout(0).build(),
                true,
            ),
            (
                "off",
                Config::builder().auto_ping(false).idle_timeout(0).build(),
                false,
            ),
        ];

        for (name, config, should_register) in configs {
            let (client_io, _server_io) = tokio::io::duplex(1024);
            let mut ws = WebSocketStream::client(client_io, config);

            assert!(poll!(std::pin::pin!(ws.next())).is_pending(), "{name}");
            assert_eq!(ws.heartbeat_sleep.is_some(), should_register, "{name}");
        }
    }

    #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
    #[tokio::test(start_paused = true)]
    async fn matching_pong_without_timeout_schedules_the_next_ping() {
        let (client_io, mut server_io) = tokio::io::duplex(1024);
        let config = Config::builder()
            .ping_interval(1)
            .pong_timeout(0)
            .idle_timeout(0)
            .build();
        let mut ws = WebSocketStream::client(client_io, config);

        assert!(poll!(std::pin::pin!(ws.next())).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(poll!(std::pin::pin!(ws.next())).is_pending());
        let payload = read_masked_control_payload(&mut server_io, 0x09).await;

        let mut pong = vec![0x8a, payload.len() as u8];
        pong.extend_from_slice(&payload);
        server_io.write_all(&pong).await.unwrap();
        assert!(matches!(
            ws.next().await,
            Some(Ok(Message::Pong(received))) if received == payload
        ));
        assert!(!ws.heartbeat.has_outstanding_ping());

        tokio::time::advance(Duration::from_millis(999)).await;
        assert!(poll!(std::pin::pin!(ws.next())).is_pending());
        let mut next_ping = [0; 14];
        assert!(poll!(std::pin::pin!(server_io.read(&mut next_ping))).is_pending());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(poll!(std::pin::pin!(ws.next())).is_pending());
        server_io.read_exact(&mut next_ping).await.unwrap();
        assert_eq!(&next_ping[..2], &[0x89, 0x88]);
    }

    #[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
    #[tokio::test(start_paused = true)]
    async fn expired_unified_deadline_precedes_a_queued_message() {
        let (client_io, _server_io) = tokio::io::duplex(1024);
        let config = Config::builder().auto_ping(false).idle_timeout(1).build();
        let mut ws = WebSocketStream::client(client_io, config);
        ws.pending_messages.push(Message::text("queued"));

        tokio::time::advance(Duration::from_millis(1001)).await;
        assert!(matches!(ws.next().await, Some(Err(Error::IdleTimeout))));
    }

    #[tokio::test]
    async fn dropping_writer_after_ready_write_cancels_reader() {
        let (client_io, mut peer_io) = tokio::io::duplex(1024);
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let (mut reader, mut writer) = WebSocketStream::client(client_io, config).split();

        writer.send_text("sent").await.unwrap();
        let payload = read_masked_control_payload(&mut peer_io, 0x01).await;
        assert_eq!(payload, b"sent");

        drop(writer);
        assert!(reader.next().await.is_none());
    }

    #[tokio::test]
    async fn parse_failure_follows_partially_decoded_messages() {
        let (io, mut peer) = tokio::io::duplex(1024);
        let mut ws = WebSocketStream::client(io, Config::default());
        // A valid binary frame followed by a reserved opcode in the same read.
        peer.write_all(b"\x82\x01x\x83\x00").await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"x");
        assert!(ws.next().await.unwrap().is_err());
        assert!(ws.next().await.is_none());
    }

    #[tokio::test]
    async fn handshake_parse_failure_follows_partially_decoded_messages() {
        let (io, _peer) = tokio::io::duplex(1024);
        let mut ws = WebSocketStream::from_raw_with_leftover(
            io,
            Role::Client,
            Config::default(),
            Some(Bytes::from_static(b"\x82\x01x\x83\x00")),
        );
        assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"x");
        assert!(ws.next().await.unwrap().is_err());
        assert!(ws.next().await.is_none());
    }
}
