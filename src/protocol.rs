//! WebSocket protocol implementation
//!
//! This module handles the WebSocket protocol state machine, including:
//! - Message fragmentation and reassembly
//! - Control frame handling (ping/pong/close)
//! - State transitions

use bytes::{Bytes, BytesMut};

use crate::error::{CloseReason, Error, Result};
#[cfg(feature = "permessage-deflate")]
use crate::frame::encode_frame_with_rsv;
use crate::frame::{Frame, FrameParser, OpCode, encode_frame};
use crate::utf8::{validate_utf8, validate_utf8_incomplete};

#[cfg(feature = "permessage-deflate")]
use crate::deflate::{DeflateConfig, DeflateContext};

/// WebSocket endpoint role
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Client (must mask frames)
    Client,
    /// Server (must not mask frames)
    Server,
}

/// WebSocket message (complete, possibly assembled from fragments)
///
/// Text messages use `Bytes` internally for zero-copy efficiency.
/// The payload is UTF-8 validated during parsing, so `as_text()` is safe.
#[derive(Debug, Clone)]
pub enum Message {
    /// Text message (UTF-8 validated, stored as Bytes for zero-copy)
    Text(Bytes),
    /// Binary message
    Binary(Bytes),
    /// Ping message
    Ping(Bytes),
    /// Pong message
    Pong(Bytes),
    /// Close message
    Close(Option<CloseReason>),
}

impl Message {
    /// Create a text message from a string
    #[inline]
    pub fn text(s: impl Into<String>) -> Self {
        Message::Text(Bytes::from(s.into()))
    }

    /// Create a binary message
    #[inline]
    pub fn binary(data: impl Into<Bytes>) -> Self {
        Message::Binary(data.into())
    }

    /// Create a ping message
    #[inline]
    pub fn ping(data: impl Into<Bytes>) -> Self {
        Message::Ping(data.into())
    }

    /// Create a pong message
    #[inline]
    pub fn pong(data: impl Into<Bytes>) -> Self {
        Message::Pong(data.into())
    }

    /// Check if this is a close message
    #[inline]
    pub fn is_close(&self) -> bool {
        matches!(self, Message::Close(_))
    }

    /// Check if this is a text message
    #[inline]
    pub fn is_text(&self) -> bool {
        matches!(self, Message::Text(_))
    }

    /// Check if this is a binary message
    #[inline]
    pub fn is_binary(&self) -> bool {
        matches!(self, Message::Binary(_))
    }

    /// Check if this is a ping message
    #[inline]
    pub fn is_ping(&self) -> bool {
        matches!(self, Message::Ping(_))
    }

    /// Check if this is a pong message
    #[inline]
    pub fn is_pong(&self) -> bool {
        matches!(self, Message::Pong(_))
    }

    /// Check if this is a control message
    #[inline]
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Message::Ping(_) | Message::Pong(_) | Message::Close(_)
        )
    }

    /// Get message as text (returns None for non-text messages)
    ///
    /// This is zero-copy - it returns a reference to the underlying bytes.
    /// The text is guaranteed to be valid UTF-8 as it was validated during parsing.
    #[inline]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Message::Text(b) => {
                // SAFETY: Text messages are UTF-8 validated during parsing
                Some(unsafe { std::str::from_utf8_unchecked(b) })
            }
            _ => None,
        }
    }

    /// Get message as bytes
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Message::Text(b) => b,
            Message::Binary(b) => b,
            Message::Ping(b) => b,
            Message::Pong(b) => b,
            Message::Close(_) => &[],
        }
    }

    /// Convert to text message (allocates a String)
    ///
    /// Returns None for non-text messages.
    pub fn into_text(self) -> Option<String> {
        match self {
            Message::Text(b) => {
                // SAFETY: Text messages are UTF-8 validated during parsing
                Some(unsafe { String::from_utf8_unchecked(b.to_vec()) })
            }
            _ => None,
        }
    }

    /// Get the underlying Bytes for text messages (zero-copy)
    ///
    /// Returns None for non-text messages.
    #[inline]
    pub fn text_bytes(&self) -> Option<&Bytes> {
        match self {
            Message::Text(b) => Some(b),
            _ => None,
        }
    }

    /// Convert to binary data
    pub fn into_bytes(self) -> Bytes {
        match self {
            Message::Text(b) => b,
            Message::Binary(b) => b,
            Message::Ping(b) => b,
            Message::Pong(b) => b,
            Message::Close(_) => Bytes::new(),
        }
    }
}

impl From<String> for Message {
    fn from(s: String) -> Self {
        Message::Text(Bytes::from(s))
    }
}

impl From<&str> for Message {
    fn from(s: &str) -> Self {
        Message::Text(Bytes::copy_from_slice(s.as_bytes()))
    }
}

impl From<Vec<u8>> for Message {
    fn from(v: Vec<u8>) -> Self {
        Message::Binary(Bytes::from(v))
    }
}

impl From<Bytes> for Message {
    fn from(b: Bytes) -> Self {
        Message::Binary(b)
    }
}

impl From<&[u8]> for Message {
    fn from(b: &[u8]) -> Self {
        Message::Binary(Bytes::copy_from_slice(b))
    }
}

/// Protocol state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Normal operation
    Open,
    /// Close frame sent, waiting for response
    CloseSent,
    /// Close frame received, sent response
    CloseReceived,
    /// Connection closed
    Closed,
}

/// WebSocket protocol handler
///
/// Handles frame parsing, message assembly, and control frame processing.
pub struct Protocol {
    /// Endpoint role
    pub(crate) role: Role,
    /// Current state
    state: State,
    /// Frame parser
    pub(crate) parser: FrameParser,
    /// Fragment buffer for message reassembly
    pub(crate) fragment_buf: BytesMut,
    /// Opcode of current fragmented message
    pub(crate) fragment_opcode: Option<OpCode>,
    /// Maximum message size
    pub(crate) max_message_size: usize,
    /// Pending close reason (if we received a close frame)
    pending_close: Option<CloseReason>,
}

impl Protocol {
    /// Create a new protocol handler
    pub fn new(role: Role, max_frame_size: usize, max_message_size: usize) -> Self {
        let expect_masked = role == Role::Server;

        Self {
            role,
            state: State::Open,
            parser: FrameParser::new(max_frame_size, expect_masked),
            fragment_buf: BytesMut::new(),
            fragment_opcode: None,
            max_message_size,
            pending_close: None,
        }
    }

    /// Check if connection is closed
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.state == State::Closed
    }

    /// Check if we're in the closing handshake
    #[inline]
    pub fn is_closing(&self) -> bool {
        matches!(self.state, State::CloseSent | State::CloseReceived)
    }

    /// Process incoming data and return complete messages
    ///
    /// This may return multiple messages if the buffer contains multiple frames.
    /// Control frames are processed inline and may be interspersed with data frames.
    #[inline]
    pub fn process(&mut self, buf: &mut BytesMut) -> Result<Vec<Message>> {
        let mut messages = Vec::new();
        self.process_into(buf, &mut messages)?;
        Ok(messages)
    }

    /// Process incoming data into a reusable message buffer (zero-allocation hot path)
    ///
    /// This variant allows reusing a Vec<Message> across calls to avoid allocations.
    #[inline]
    pub fn process_into(&mut self, buf: &mut BytesMut, messages: &mut Vec<Message>) -> Result<()> {
        messages.clear();

        while !buf.is_empty() {
            match self.parser.parse(buf)? {
                Some(frame) => {
                    if let Some(msg) = self.handle_frame(frame)? {
                        messages.push(msg);
                    }
                }
                None => break,
            }
        }

        Ok(())
    }

    /// Handle a single parsed frame
    fn handle_frame(&mut self, frame: Frame) -> Result<Option<Message>> {
        match frame.header.opcode {
            OpCode::Continuation => self.handle_continuation(frame),
            OpCode::Text => self.handle_text(frame),
            OpCode::Binary => self.handle_binary(frame),
            OpCode::Close => self.handle_close(frame),
            OpCode::Ping => self.handle_ping(frame),
            OpCode::Pong => self.handle_pong(frame),
        }
    }

    /// Handle text frame
    fn handle_text(&mut self, frame: Frame) -> Result<Option<Message>> {
        if self.fragment_opcode.is_some() {
            return Err(Error::Protocol("expected continuation frame"));
        }

        if frame.header.fin {
            // Complete message in one frame (fast path)
            if !validate_utf8(&frame.payload) {
                return Err(Error::InvalidUtf8);
            }
            // Zero-copy: just return the Bytes directly (already UTF-8 validated)
            Ok(Some(Message::Text(frame.payload)))
        } else {
            // Start of fragmented message
            self.start_fragment(OpCode::Text, frame.payload)?;
            Ok(None)
        }
    }

    /// Handle binary frame
    fn handle_binary(&mut self, frame: Frame) -> Result<Option<Message>> {
        if self.fragment_opcode.is_some() {
            return Err(Error::Protocol("expected continuation frame"));
        }

        if frame.header.fin {
            // Complete message in one frame (fast path)
            Ok(Some(Message::Binary(frame.payload)))
        } else {
            // Start of fragmented message
            self.start_fragment(OpCode::Binary, frame.payload)?;
            Ok(None)
        }
    }

    /// Handle continuation frame
    fn handle_continuation(&mut self, frame: Frame) -> Result<Option<Message>> {
        let opcode = self
            .fragment_opcode
            .ok_or(Error::Protocol("unexpected continuation frame"))?;

        // Check total size
        let new_size = self.fragment_buf.len() + frame.payload.len();
        if new_size > self.max_message_size {
            return Err(Error::MessageTooLarge);
        }

        self.fragment_buf.extend_from_slice(&frame.payload);

        if frame.header.fin {
            // Complete the fragmented message
            self.complete_fragment(opcode)
        } else {
            // Validate partial UTF-8 for text messages
            if opcode == OpCode::Text {
                let (valid, _incomplete) = validate_utf8_incomplete(&self.fragment_buf);
                if !valid {
                    return Err(Error::InvalidUtf8);
                }
            }
            Ok(None)
        }
    }

    /// Start a fragmented message
    pub(crate) fn start_fragment(&mut self, opcode: OpCode, payload: Bytes) -> Result<()> {
        if payload.len() > self.max_message_size {
            return Err(Error::MessageTooLarge);
        }

        self.fragment_opcode = Some(opcode);
        self.fragment_buf.clear();
        self.fragment_buf.extend_from_slice(&payload);

        // Validate partial UTF-8 for text messages
        if opcode == OpCode::Text {
            let (valid, _incomplete) = validate_utf8_incomplete(&self.fragment_buf);
            if !valid {
                return Err(Error::InvalidUtf8);
            }
        }

        Ok(())
    }

    /// Complete a fragmented message
    fn complete_fragment(&mut self, opcode: OpCode) -> Result<Option<Message>> {
        self.fragment_opcode = None;
        let data = self.fragment_buf.split().freeze();

        match opcode {
            OpCode::Text => {
                if !validate_utf8(&data) {
                    return Err(Error::InvalidUtf8);
                }
                // Zero-copy: just return the Bytes directly (already UTF-8 validated)
                Ok(Some(Message::Text(data)))
            }
            OpCode::Binary => Ok(Some(Message::Binary(data))),
            _ => Err(Error::Protocol("invalid fragment opcode")),
        }
    }

    /// Handle close frame
    pub(crate) fn handle_close(&mut self, frame: Frame) -> Result<Option<Message>> {
        let reason = if frame.payload.len() >= 2 {
            let code = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);

            // Validate close code
            if !CloseReason::is_valid_code(code) && !(3000..=4999).contains(&code) {
                return Err(Error::InvalidCloseCode(code));
            }

            let reason_text = if frame.payload.len() > 2 {
                let text = &frame.payload[2..];
                if !validate_utf8(text) {
                    return Err(Error::InvalidUtf8);
                }
                String::from_utf8_lossy(text).into_owned()
            } else {
                String::new()
            };

            Some(CloseReason::new(code, reason_text))
        } else if frame.payload.is_empty() {
            None
        } else {
            // Close frame with 1 byte payload is invalid
            return Err(Error::Protocol("invalid close frame payload"));
        };

        match self.state {
            State::Open => {
                self.state = State::CloseReceived;
                self.pending_close = reason.clone();
            }
            State::CloseSent => {
                self.state = State::Closed;
            }
            _ => {}
        }

        Ok(Some(Message::Close(reason)))
    }

    /// Handle ping frame
    pub(crate) fn handle_ping(&mut self, frame: Frame) -> Result<Option<Message>> {
        Ok(Some(Message::Ping(frame.payload)))
    }

    /// Handle pong frame
    pub(crate) fn handle_pong(&mut self, frame: Frame) -> Result<Option<Message>> {
        Ok(Some(Message::Pong(frame.payload)))
    }

    /// Encode a message for sending
    pub fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()> {
        let mask = if self.role == Role::Client {
            Some(crate::mask::generate_mask())
        } else {
            None
        };

        match msg {
            Message::Text(b) => {
                encode_frame(buf, OpCode::Text, b, true, mask);
            }
            Message::Binary(b) => {
                encode_frame(buf, OpCode::Binary, b, true, mask);
            }
            Message::Ping(b) => {
                encode_frame(buf, OpCode::Ping, b, true, mask);
            }
            Message::Pong(b) => {
                encode_frame(buf, OpCode::Pong, b, true, mask);
            }
            Message::Close(reason) => {
                if self.state == State::Open {
                    self.state = State::CloseSent;
                }

                let payload = if let Some(r) = reason {
                    let mut p = BytesMut::with_capacity(2 + r.reason.len());
                    p.extend_from_slice(&r.code.to_be_bytes());
                    p.extend_from_slice(r.reason.as_bytes());
                    p.freeze()
                } else {
                    Bytes::new()
                };

                encode_frame(buf, OpCode::Close, &payload, true, mask);
            }
        }

        Ok(())
    }

    /// Encode a pong response for a ping
    pub fn encode_pong(&mut self, ping_data: &[u8], buf: &mut BytesMut) {
        let mask = if self.role == Role::Client {
            Some(crate::mask::generate_mask())
        } else {
            None
        };
        encode_frame(buf, OpCode::Pong, ping_data, true, mask);
    }

    /// Encode a close response
    pub fn encode_close_response(&mut self, buf: &mut BytesMut) {
        let mask = if self.role == Role::Client {
            Some(crate::mask::generate_mask())
        } else {
            None
        };

        let payload = if let Some(ref reason) = self.pending_close {
            let mut p = BytesMut::with_capacity(2 + reason.reason.len());
            p.extend_from_slice(&reason.code.to_be_bytes());
            p.extend_from_slice(reason.reason.as_bytes());
            p.freeze()
        } else {
            Bytes::new()
        };

        encode_frame(buf, OpCode::Close, &payload, true, mask);

        if self.state == State::CloseReceived {
            self.state = State::Closed;
        }
    }

    /// Enable compression support in the frame parser
    #[cfg(feature = "permessage-deflate")]
    pub fn enable_compression(&mut self) {
        self.parser.set_compression(true);
    }
}

/// WebSocket protocol handler with permessage-deflate compression (RFC 7692)
#[cfg(feature = "permessage-deflate")]
pub struct CompressedProtocol {
    /// Base protocol handler
    inner: Protocol,
    /// Deflate compression context
    deflate: DeflateContext,
    /// Whether the current fragmented message is compressed
    fragment_compressed: bool,
    /// Buffer for decompressed fragment data
    decompress_buf: BytesMut,
}

#[cfg(feature = "permessage-deflate")]
impl CompressedProtocol {
    /// Create a new compressed protocol handler for server role
    pub fn server(max_frame_size: usize, max_message_size: usize, config: DeflateConfig) -> Self {
        let mut inner = Protocol::new(Role::Server, max_frame_size, max_message_size);
        inner.enable_compression();

        Self {
            inner,
            deflate: DeflateContext::server(config),
            fragment_compressed: false,
            decompress_buf: BytesMut::new(),
        }
    }

    /// Create a new compressed protocol handler for client role
    pub fn client(max_frame_size: usize, max_message_size: usize, config: DeflateConfig) -> Self {
        let mut inner = Protocol::new(Role::Client, max_frame_size, max_message_size);
        inner.enable_compression();

        Self {
            inner,
            deflate: DeflateContext::client(config),
            fragment_compressed: false,
            decompress_buf: BytesMut::new(),
        }
    }

    /// Check if connection is closed
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Check if we're in the closing handshake
    #[inline]
    pub fn is_closing(&self) -> bool {
        self.inner.is_closing()
    }

    /// Process incoming data and return complete messages
    pub fn process(&mut self, buf: &mut BytesMut) -> Result<Vec<Message>> {
        let mut messages = Vec::new();
        self.process_into(buf, &mut messages)?;
        Ok(messages)
    }

    /// Process incoming data into a reusable message buffer
    #[inline]
    pub fn process_into(&mut self, buf: &mut BytesMut, messages: &mut Vec<Message>) -> Result<()> {
        const DEBUG: bool = false;
        messages.clear();

        while !buf.is_empty() {
            if DEBUG {
                eprintln!("[PROTOCOL] process_into loop: buf has {} bytes", buf.len());
            }
            match self.inner.parser.parse(buf)? {
                Some(frame) => {
                    if DEBUG {
                        eprintln!("[PROTOCOL] Parsed frame, handling...");
                    }
                    if let Some(msg) = self.handle_frame(frame)? {
                        messages.push(msg);
                        if DEBUG {
                            eprintln!("[PROTOCOL] Added message to output");
                        }
                    } else if DEBUG {
                        eprintln!("[PROTOCOL] No message from handle_frame (fragment or control)");
                    }
                }
                None => {
                    if DEBUG {
                        eprintln!("[PROTOCOL] Parser returned None, breaking loop");
                    }
                    break;
                }
            }
        }

        if DEBUG {
            eprintln!("[PROTOCOL] process_into done, {} messages", messages.len());
        }

        Ok(())
    }

    /// Handle a parsed frame with decompression support
    fn handle_frame(&mut self, frame: Frame) -> Result<Option<Message>> {
        let is_compressed = frame.header.rsv1;

        match frame.header.opcode {
            OpCode::Continuation => self.handle_continuation(frame),
            OpCode::Text => self.handle_text(frame, is_compressed),
            OpCode::Binary => self.handle_binary(frame, is_compressed),
            OpCode::Close => self.inner.handle_close(frame),
            OpCode::Ping => self.inner.handle_ping(frame),
            OpCode::Pong => self.inner.handle_pong(frame),
        }
    }

    /// Handle text frame with potential decompression
    fn handle_text(&mut self, frame: Frame, compressed: bool) -> Result<Option<Message>> {
        const DEBUG: bool = false;

        if self.inner.fragment_opcode.is_some() {
            return Err(Error::Protocol("expected continuation frame"));
        }

        if frame.header.fin {
            // Complete message in one frame
            if DEBUG {
                eprintln!(
                    "[PROTOCOL] Complete text message, compressed={}, size={}",
                    compressed,
                    frame.payload.len()
                );
            }
            let payload = if compressed {
                self.deflate
                    .decompress(&frame.payload, self.inner.max_message_size)?
            } else {
                frame.payload
            };

            if !validate_utf8(&payload) {
                return Err(Error::InvalidUtf8);
            }
            // Zero-copy: just return the Bytes directly (already UTF-8 validated)
            Ok(Some(Message::Text(payload)))
        } else {
            // Start of fragmented message
            if DEBUG {
                eprintln!(
                    "[PROTOCOL] Starting fragmented text message, compressed={}, first fragment size={}",
                    compressed,
                    frame.payload.len()
                );
            }
            self.fragment_compressed = compressed;

            // For compressed fragments, store as binary (don't validate UTF-8 yet)
            // We'll decompress and validate when the message is complete
            if compressed {
                self.inner.start_fragment(OpCode::Binary, frame.payload)?;
                // Override the opcode back to Text for proper handling
                self.inner.fragment_opcode = Some(OpCode::Text);
            } else {
                self.inner.start_fragment(OpCode::Text, frame.payload)?;
            }
            Ok(None)
        }
    }

    /// Handle binary frame with potential decompression
    fn handle_binary(&mut self, frame: Frame, compressed: bool) -> Result<Option<Message>> {
        if self.inner.fragment_opcode.is_some() {
            return Err(Error::Protocol("expected continuation frame"));
        }

        if frame.header.fin {
            // Complete message in one frame
            let payload = if compressed {
                self.deflate
                    .decompress(&frame.payload, self.inner.max_message_size)?
            } else {
                frame.payload
            };
            Ok(Some(Message::Binary(payload)))
        } else {
            // Start of fragmented message
            self.fragment_compressed = compressed;
            self.inner.start_fragment(OpCode::Binary, frame.payload)?;
            Ok(None)
        }
    }

    /// Handle continuation frame
    fn handle_continuation(&mut self, frame: Frame) -> Result<Option<Message>> {
        let opcode = self
            .inner
            .fragment_opcode
            .ok_or(Error::Protocol("unexpected continuation frame"))?;

        // Check total size
        let new_size = self.inner.fragment_buf.len() + frame.payload.len();
        if new_size > self.inner.max_message_size {
            return Err(Error::MessageTooLarge);
        }

        self.inner.fragment_buf.extend_from_slice(&frame.payload);

        if frame.header.fin {
            // Complete the fragmented message
            self.complete_fragment(opcode)
        } else {
            Ok(None)
        }
    }

    /// Complete a fragmented message with decompression if needed
    fn complete_fragment(&mut self, opcode: OpCode) -> Result<Option<Message>> {
        self.inner.fragment_opcode = None;
        let compressed_data = self.inner.fragment_buf.split().freeze();

        // Decompress if the first frame had RSV1 set
        let data = if self.fragment_compressed {
            self.fragment_compressed = false;
            self.deflate
                .decompress(&compressed_data, self.inner.max_message_size)?
        } else {
            compressed_data
        };

        match opcode {
            OpCode::Text => {
                if !validate_utf8(&data) {
                    return Err(Error::InvalidUtf8);
                }
                // Zero-copy: just return the Bytes directly (already UTF-8 validated)
                Ok(Some(Message::Text(data)))
            }
            OpCode::Binary => Ok(Some(Message::Binary(data))),
            _ => Err(Error::Protocol("invalid fragment opcode")),
        }
    }

    /// Encode a message for sending with compression
    pub fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()> {
        let mask = if self.inner.role == Role::Client {
            Some(crate::mask::generate_mask())
        } else {
            None
        };

        match msg {
            Message::Text(b) => {
                // Try to compress
                if let Some(compressed) = self.deflate.compress(b)? {
                    encode_frame_with_rsv(buf, OpCode::Text, &compressed, true, mask, true);
                } else {
                    encode_frame(buf, OpCode::Text, b, true, mask);
                }
            }
            Message::Binary(b) => {
                // Try to compress
                if let Some(compressed) = self.deflate.compress(b)? {
                    encode_frame_with_rsv(buf, OpCode::Binary, &compressed, true, mask, true);
                } else {
                    encode_frame(buf, OpCode::Binary, b, true, mask);
                }
            }
            Message::Ping(b) => {
                // Control frames are never compressed
                encode_frame(buf, OpCode::Ping, b, true, mask);
            }
            Message::Pong(b) => {
                encode_frame(buf, OpCode::Pong, b, true, mask);
            }
            Message::Close(_) => {
                self.inner.encode_message(msg, buf)?;
            }
        }

        Ok(())
    }

    /// Encode a pong response for a ping
    pub fn encode_pong(&mut self, ping_data: &[u8], buf: &mut BytesMut) {
        self.inner.encode_pong(ping_data, buf);
    }

    /// Encode a close response
    pub fn encode_close_response(&mut self, buf: &mut BytesMut) {
        self.inner.encode_close_response(buf);
    }

    /// Split the compressed protocol into separate reader and writer halves
    ///
    /// This allows the encoder and decoder to be used independently for
    /// concurrent read/write operations.
    pub fn split(
        self,
        max_frame_size: usize,
        max_message_size: usize,
    ) -> (CompressedReaderProtocol, CompressedWriterProtocol) {
        let role = self.inner.role;

        // Create fresh reader protocol (decoder state)
        let reader = CompressedReaderProtocol {
            role,
            parser: FrameParser::new(max_frame_size, role == Role::Server),
            fragment_buf: self.inner.fragment_buf,
            fragment_opcode: self.inner.fragment_opcode,
            max_message_size,
            decoder: self.deflate.decoder,
            fragment_compressed: self.fragment_compressed,
        };

        // Create fresh writer protocol (encoder state)
        let writer = CompressedWriterProtocol {
            role,
            encoder: self.deflate.encoder,
        };

        (reader, writer)
    }
}

/// Reader half of a split compressed protocol
///
/// Contains the decoder and frame parser for reading compressed messages.
#[cfg(feature = "permessage-deflate")]
pub struct CompressedReaderProtocol {
    /// Endpoint role
    role: Role,
    /// Frame parser
    parser: FrameParser,
    /// Fragment buffer for message reassembly
    fragment_buf: BytesMut,
    /// Opcode of current fragmented message
    fragment_opcode: Option<OpCode>,
    /// Maximum message size
    max_message_size: usize,
    /// Deflate decoder
    decoder: crate::deflate::DeflateDecoder,
    /// Whether the current fragmented message is compressed
    fragment_compressed: bool,
}

#[cfg(feature = "permessage-deflate")]
impl CompressedReaderProtocol {
    /// Create a new reader protocol for server role
    pub fn server(max_frame_size: usize, max_message_size: usize, config: &DeflateConfig) -> Self {
        Self {
            role: Role::Server,
            parser: FrameParser::new(max_frame_size, true),
            fragment_buf: BytesMut::new(),
            fragment_opcode: None,
            max_message_size,
            decoder: crate::deflate::DeflateDecoder::new(
                config.client_max_window_bits,
                config.client_no_context_takeover,
            ),
            fragment_compressed: false,
        }
    }

    /// Create a new reader protocol for client role
    pub fn client(max_frame_size: usize, max_message_size: usize, config: &DeflateConfig) -> Self {
        Self {
            role: Role::Client,
            parser: FrameParser::new(max_frame_size, false),
            fragment_buf: BytesMut::new(),
            fragment_opcode: None,
            max_message_size,
            decoder: crate::deflate::DeflateDecoder::new(
                config.server_max_window_bits,
                config.server_no_context_takeover,
            ),
            fragment_compressed: false,
        }
    }

    /// Process incoming data and return complete messages
    pub fn process(&mut self, buf: &mut BytesMut) -> Result<Vec<Message>> {
        let mut messages = Vec::new();
        self.process_into(buf, &mut messages)?;
        Ok(messages)
    }

    /// Process incoming data into a reusable message buffer
    pub fn process_into(&mut self, buf: &mut BytesMut, messages: &mut Vec<Message>) -> Result<()> {
        messages.clear();

        // Enable compression in parser
        self.parser.set_compression(true);

        while !buf.is_empty() {
            match self.parser.parse(buf)? {
                Some(frame) => {
                    if let Some(msg) = self.handle_frame(frame)? {
                        messages.push(msg);
                    }
                }
                None => break,
            }
        }

        Ok(())
    }

    /// Handle a parsed frame with decompression support
    fn handle_frame(&mut self, frame: Frame) -> Result<Option<Message>> {
        let is_compressed = frame.header.rsv1;

        match frame.header.opcode {
            OpCode::Continuation => self.handle_continuation(frame),
            OpCode::Text => self.handle_text(frame, is_compressed),
            OpCode::Binary => self.handle_binary(frame, is_compressed),
            OpCode::Close => self.handle_close(frame),
            OpCode::Ping => Ok(Some(Message::Ping(frame.payload))),
            OpCode::Pong => Ok(Some(Message::Pong(frame.payload))),
        }
    }

    /// Handle text frame with potential decompression
    fn handle_text(&mut self, frame: Frame, compressed: bool) -> Result<Option<Message>> {
        if self.fragment_opcode.is_some() {
            return Err(Error::Protocol("expected continuation frame"));
        }

        if frame.header.fin {
            let payload = if compressed {
                self.decoder
                    .decompress(&frame.payload, self.max_message_size)?
            } else {
                frame.payload
            };

            if !validate_utf8(&payload) {
                return Err(Error::InvalidUtf8);
            }
            Ok(Some(Message::Text(payload)))
        } else {
            self.fragment_compressed = compressed;
            self.start_fragment(OpCode::Text, frame.payload)?;
            Ok(None)
        }
    }

    /// Handle binary frame with potential decompression
    fn handle_binary(&mut self, frame: Frame, compressed: bool) -> Result<Option<Message>> {
        if self.fragment_opcode.is_some() {
            return Err(Error::Protocol("expected continuation frame"));
        }

        if frame.header.fin {
            let payload = if compressed {
                self.decoder
                    .decompress(&frame.payload, self.max_message_size)?
            } else {
                frame.payload
            };
            Ok(Some(Message::Binary(payload)))
        } else {
            self.fragment_compressed = compressed;
            self.start_fragment(OpCode::Binary, frame.payload)?;
            Ok(None)
        }
    }

    /// Handle continuation frame
    fn handle_continuation(&mut self, frame: Frame) -> Result<Option<Message>> {
        let opcode = self
            .fragment_opcode
            .ok_or(Error::Protocol("unexpected continuation frame"))?;

        let new_size = self.fragment_buf.len() + frame.payload.len();
        if new_size > self.max_message_size {
            return Err(Error::MessageTooLarge);
        }

        self.fragment_buf.extend_from_slice(&frame.payload);

        if frame.header.fin {
            self.complete_fragment(opcode)
        } else {
            Ok(None)
        }
    }

    /// Start a fragmented message
    fn start_fragment(&mut self, opcode: OpCode, payload: Bytes) -> Result<()> {
        if payload.len() > self.max_message_size {
            return Err(Error::MessageTooLarge);
        }

        self.fragment_opcode = Some(opcode);
        self.fragment_buf.clear();
        self.fragment_buf.extend_from_slice(&payload);
        Ok(())
    }

    /// Complete a fragmented message with decompression if needed
    fn complete_fragment(&mut self, opcode: OpCode) -> Result<Option<Message>> {
        self.fragment_opcode = None;
        let compressed_data = self.fragment_buf.split().freeze();

        let data = if self.fragment_compressed {
            self.fragment_compressed = false;
            self.decoder
                .decompress(&compressed_data, self.max_message_size)?
        } else {
            compressed_data
        };

        match opcode {
            OpCode::Text => {
                if !validate_utf8(&data) {
                    return Err(Error::InvalidUtf8);
                }
                Ok(Some(Message::Text(data)))
            }
            OpCode::Binary => Ok(Some(Message::Binary(data))),
            _ => Err(Error::Protocol("invalid fragment opcode")),
        }
    }

    /// Handle close frame
    fn handle_close(&mut self, frame: Frame) -> Result<Option<Message>> {
        let reason = if frame.payload.len() >= 2 {
            let code = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);

            if !CloseReason::is_valid_code(code) && !(3000..=4999).contains(&code) {
                return Err(Error::InvalidCloseCode(code));
            }

            let reason_text = if frame.payload.len() > 2 {
                let text = &frame.payload[2..];
                if !validate_utf8(text) {
                    return Err(Error::InvalidUtf8);
                }
                String::from_utf8_lossy(text).into_owned()
            } else {
                String::new()
            };

            Some(CloseReason::new(code, reason_text))
        } else if frame.payload.is_empty() {
            None
        } else {
            return Err(Error::Protocol("invalid close frame payload"));
        };

        Ok(Some(Message::Close(reason)))
    }
}

/// Writer half of a split compressed protocol
///
/// Contains the encoder for writing compressed messages.
#[cfg(feature = "permessage-deflate")]
pub struct CompressedWriterProtocol {
    /// Endpoint role
    role: Role,
    /// Deflate encoder
    encoder: crate::deflate::DeflateEncoder,
}

#[cfg(feature = "permessage-deflate")]
impl CompressedWriterProtocol {
    /// Create a new writer protocol for server role
    pub fn server(config: &DeflateConfig) -> Self {
        Self {
            role: Role::Server,
            encoder: crate::deflate::DeflateEncoder::new(
                config.server_max_window_bits,
                config.server_no_context_takeover,
                config.compression_level,
                config.compression_threshold,
            ),
        }
    }

    /// Create a new writer protocol for client role
    pub fn client(config: &DeflateConfig) -> Self {
        Self {
            role: Role::Client,
            encoder: crate::deflate::DeflateEncoder::new(
                config.client_max_window_bits,
                config.client_no_context_takeover,
                config.compression_level,
                config.compression_threshold,
            ),
        }
    }

    /// Encode a message for sending with compression
    pub fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()> {
        let mask = if self.role == Role::Client {
            Some(crate::mask::generate_mask())
        } else {
            None
        };

        match msg {
            Message::Text(b) => {
                if let Some(compressed) = self.encoder.compress(b)? {
                    encode_frame_with_rsv(buf, OpCode::Text, &compressed, true, mask, true);
                } else {
                    encode_frame(buf, OpCode::Text, b, true, mask);
                }
            }
            Message::Binary(b) => {
                if let Some(compressed) = self.encoder.compress(b)? {
                    encode_frame_with_rsv(buf, OpCode::Binary, &compressed, true, mask, true);
                } else {
                    encode_frame(buf, OpCode::Binary, b, true, mask);
                }
            }
            Message::Ping(b) => {
                encode_frame(buf, OpCode::Ping, b, true, mask);
            }
            Message::Pong(b) => {
                encode_frame(buf, OpCode::Pong, b, true, mask);
            }
            Message::Close(reason) => {
                let payload = if let Some(r) = reason {
                    let mut p = BytesMut::with_capacity(2 + r.reason.len());
                    p.extend_from_slice(&r.code.to_be_bytes());
                    p.extend_from_slice(r.reason.as_bytes());
                    p.freeze()
                } else {
                    Bytes::new()
                };
                encode_frame(buf, OpCode::Close, &payload, true, mask);
            }
        }

        Ok(())
    }

    /// Encode a pong response for a ping
    pub fn encode_pong(&mut self, ping_data: &[u8], buf: &mut BytesMut) {
        let mask = if self.role == Role::Client {
            Some(crate::mask::generate_mask())
        } else {
            None
        };
        encode_frame(buf, OpCode::Pong, ping_data, true, mask);
    }

    /// Encode a close response
    pub fn encode_close_response(&mut self, buf: &mut BytesMut) {
        let mask = if self.role == Role::Client {
            Some(crate::mask::generate_mask())
        } else {
            None
        };
        encode_frame(buf, OpCode::Close, &[], true, mask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_text() {
        let mut protocol = Protocol::new(Role::Server, 1024 * 1024, 64 * 1024 * 1024);

        // Simulate receiving a text frame
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[0x81, 0x85]); // FIN + Text, Masked + len 5
        buf.extend_from_slice(&[0x37, 0xfa, 0x21, 0x3d]); // Mask

        // "Hello" XORed with mask
        let mut payload = *b"Hello";
        crate::simd::apply_mask(&mut payload, [0x37, 0xfa, 0x21, 0x3d]);
        buf.extend_from_slice(&payload);

        let messages = protocol.process(&mut buf).unwrap();
        assert_eq!(messages.len(), 1);

        if let Message::Text(s) = &messages[0] {
            assert_eq!(s, "Hello");
        } else {
            panic!("Expected text message");
        }
    }

    #[test]
    fn test_fragmented_message() {
        let mut protocol = Protocol::new(Role::Server, 1024 * 1024, 64 * 1024 * 1024);

        // First fragment
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[0x01, 0x83]); // Text (not FIN), Masked + len 3
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // Zero mask
        buf.extend_from_slice(b"Hel");

        let messages = protocol.process(&mut buf).unwrap();
        assert!(messages.is_empty());

        // Final fragment
        buf.extend_from_slice(&[0x80, 0x82]); // Continuation + FIN, Masked + len 2
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // Zero mask
        buf.extend_from_slice(b"lo");

        let messages = protocol.process(&mut buf).unwrap();
        assert_eq!(messages.len(), 1);

        if let Message::Text(s) = &messages[0] {
            assert_eq!(s, "Hello");
        } else {
            panic!("Expected text message");
        }
    }

    #[test]
    fn test_encode_message() {
        let mut protocol = Protocol::new(Role::Server, 1024 * 1024, 64 * 1024 * 1024);
        let mut buf = BytesMut::new();

        protocol
            .encode_message(&Message::text("test"), &mut buf)
            .unwrap();

        assert_eq!(buf[0], 0x81); // FIN + Text
        assert_eq!(buf[1], 0x04); // Length 4 (no mask for server)
        assert_eq!(&buf[2..], b"test");
    }
}
