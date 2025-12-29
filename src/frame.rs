//! WebSocket frame parsing and serialization
//!
//! This module implements RFC 6455 WebSocket frame handling with:
//! - Zero-copy parsing using buffer views
//! - Fast-path for small messages (< 126 bytes)
//! - SIMD-accelerated masking
//! - Minimal allocations in the hot path

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::error::{CloseReason, Error, Result};
use crate::simd::apply_mask;
use crate::utf8::validate_utf8;
use crate::{MEDIUM_MESSAGE_THRESHOLD, SMALL_MESSAGE_THRESHOLD};

/// WebSocket opcode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    /// Continuation frame
    Continuation = 0x0,
    /// Text frame
    Text = 0x1,
    /// Binary frame
    Binary = 0x2,
    /// Connection close
    Close = 0x8,
    /// Ping
    Ping = 0x9,
    /// Pong
    Pong = 0xA,
}

impl OpCode {
    /// Parse opcode from byte
    #[inline]
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0x0 => Some(OpCode::Continuation),
            0x1 => Some(OpCode::Text),
            0x2 => Some(OpCode::Binary),
            0x8 => Some(OpCode::Close),
            0x9 => Some(OpCode::Ping),
            0xA => Some(OpCode::Pong),
            _ => None,
        }
    }

    /// Check if this is a control frame
    #[inline]
    pub fn is_control(&self) -> bool {
        (*self as u8) >= 0x8
    }

    /// Check if this is a data frame
    #[inline]
    pub fn is_data(&self) -> bool {
        (*self as u8) <= 0x2
    }
}

/// A parsed WebSocket frame header
#[derive(Debug, Clone)]
pub struct FrameHeader {
    /// Final fragment flag
    pub fin: bool,
    /// RSV1 (used for compression)
    pub rsv1: bool,
    /// RSV2 (reserved)
    pub rsv2: bool,
    /// RSV3 (reserved)
    pub rsv3: bool,
    /// Frame opcode
    pub opcode: OpCode,
    /// Mask flag (must be true for client->server)
    pub masked: bool,
    /// Payload length
    pub payload_len: u64,
    /// Masking key (if masked)
    pub mask: Option<[u8; 4]>,
}

impl FrameHeader {
    /// Get the total header size in bytes
    #[inline]
    pub fn header_size(&self) -> usize {
        let mut size = 2; // Base header

        // Extended payload length
        if self.payload_len > MEDIUM_MESSAGE_THRESHOLD as u64 {
            size += 8;
        } else if self.payload_len > SMALL_MESSAGE_THRESHOLD as u64 {
            size += 2;
        }

        // Mask
        if self.masked {
            size += 4;
        }

        size
    }

    /// Encode the frame header into a buffer
    #[inline]
    pub fn encode(&self, buf: &mut BytesMut) {
        // First byte: FIN, RSV1-3, opcode
        let mut b0 = self.opcode as u8;
        if self.fin {
            b0 |= 0x80;
        }
        if self.rsv1 {
            b0 |= 0x40;
        }
        if self.rsv2 {
            b0 |= 0x20;
        }
        if self.rsv3 {
            b0 |= 0x10;
        }
        buf.put_u8(b0);

        // Second byte: mask flag, payload length
        let mask_bit = if self.masked { 0x80 } else { 0x00 };

        if self.payload_len <= SMALL_MESSAGE_THRESHOLD as u64 {
            buf.put_u8(mask_bit | self.payload_len as u8);
        } else if self.payload_len <= MEDIUM_MESSAGE_THRESHOLD as u64 {
            buf.put_u8(mask_bit | 126);
            buf.put_u16(self.payload_len as u16);
        } else {
            buf.put_u8(mask_bit | 127);
            buf.put_u64(self.payload_len);
        }

        // Masking key
        if let Some(mask) = self.mask {
            buf.put_slice(&mask);
        }
    }
}

/// A complete WebSocket frame
#[derive(Debug, Clone)]
pub struct Frame {
    /// Frame header
    pub header: FrameHeader,
    /// Frame payload (already unmasked)
    pub payload: Bytes,
}

impl Frame {
    /// Create a new frame
    pub fn new(opcode: OpCode, payload: Bytes, fin: bool) -> Self {
        Self {
            header: FrameHeader {
                fin,
                rsv1: false,
                rsv2: false,
                rsv3: false,
                opcode,
                masked: false,
                payload_len: payload.len() as u64,
                mask: None,
            },
            payload,
        }
    }

    /// Create a text frame
    #[inline]
    pub fn text(data: impl Into<Bytes>) -> Self {
        Self::new(OpCode::Text, data.into(), true)
    }

    /// Create a binary frame
    #[inline]
    pub fn binary(data: impl Into<Bytes>) -> Self {
        Self::new(OpCode::Binary, data.into(), true)
    }

    /// Create a ping frame
    #[inline]
    pub fn ping(data: impl Into<Bytes>) -> Self {
        Self::new(OpCode::Ping, data.into(), true)
    }

    /// Create a pong frame
    #[inline]
    pub fn pong(data: impl Into<Bytes>) -> Self {
        Self::new(OpCode::Pong, data.into(), true)
    }

    /// Create a close frame
    #[inline]
    pub fn close(code: u16, reason: &str) -> Self {
        let mut payload = BytesMut::with_capacity(2 + reason.len());
        payload.put_u16(code);
        payload.put_slice(reason.as_bytes());
        Self::new(OpCode::Close, payload.freeze(), true)
    }

    /// Create an empty close frame
    #[inline]
    pub fn close_empty() -> Self {
        Self::new(OpCode::Close, Bytes::new(), true)
    }

    /// Check if this is a control frame
    #[inline]
    pub fn is_control(&self) -> bool {
        self.header.opcode.is_control()
    }

    /// Check if this is the final fragment
    #[inline]
    pub fn is_final(&self) -> bool {
        self.header.fin
    }

    /// Get the payload as a string (for text frames)
    pub fn as_text(&self) -> Result<&str> {
        if !validate_utf8(&self.payload) {
            return Err(Error::InvalidUtf8);
        }
        // SAFETY: We just validated UTF-8
        Ok(unsafe { std::str::from_utf8_unchecked(&self.payload) })
    }

    /// Parse close frame payload
    pub fn parse_close(&self) -> Option<CloseReason> {
        if self.payload.len() < 2 {
            return None;
        }
        let code = u16::from_be_bytes([self.payload[0], self.payload[1]]);
        let reason = if self.payload.len() > 2 {
            String::from_utf8_lossy(&self.payload[2..]).into_owned()
        } else {
            String::new()
        };
        Some(CloseReason::new(code, reason))
    }
}

/// Frame parser state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    /// Waiting for header bytes
    Header,
    /// Waiting for extended payload length (2 bytes)
    ExtendedLen16,
    /// Waiting for extended payload length (8 bytes)
    ExtendedLen64,
    /// Waiting for mask (4 bytes)
    Mask,
    /// Waiting for payload
    Payload,
}

/// High-performance frame parser
///
/// Designed for zero-copy parsing with minimal allocations.
/// Uses a state machine to handle partial reads efficiently.
pub struct FrameParser {
    state: ParseState,
    /// Partial header data
    header_buf: [u8; 14],
    header_len: usize,
    /// Parsed header (once complete)
    header: Option<FrameHeader>,
    /// Maximum frame size
    max_frame_size: usize,
    /// Whether to expect masked frames (server mode)
    expect_masked: bool,
    /// Whether RSV1 is allowed (compression enabled)
    allow_rsv1: bool,
}

impl FrameParser {
    /// Create a new frame parser
    pub fn new(max_frame_size: usize, expect_masked: bool) -> Self {
        Self {
            state: ParseState::Header,
            header_buf: [0; 14],
            header_len: 0,
            header: None,
            max_frame_size,
            expect_masked,
            allow_rsv1: false,
        }
    }

    /// Create a new frame parser with compression support
    pub fn with_compression(max_frame_size: usize, expect_masked: bool) -> Self {
        Self {
            state: ParseState::Header,
            header_buf: [0; 14],
            header_len: 0,
            header: None,
            max_frame_size,
            expect_masked,
            allow_rsv1: true,
        }
    }

    /// Enable or disable RSV1 (compression) support
    pub fn set_compression(&mut self, enabled: bool) {
        self.allow_rsv1 = enabled;
    }

    /// Reset parser state for next frame
    #[inline]
    fn reset(&mut self) {
        self.state = ParseState::Header;
        self.header_len = 0;
        self.header = None;
    }

    /// Parse a frame from the buffer
    ///
    /// Returns:
    /// - Ok(Some(frame)) if a complete frame was parsed
    /// - Ok(None) if more data is needed
    /// - Err(e) if parsing failed
    pub fn parse(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>> {
        const DEBUG: bool = false;
        loop {
            if DEBUG && buf.len() > 0 {
                eprintln!(
                    "[PARSER] State: {:?}, buf_len: {}, header_len: {}",
                    self.state,
                    buf.len(),
                    self.header_len
                );
            }
            match self.state {
                ParseState::Header => {
                    if buf.len() < 2 {
                        return Ok(None);
                    }

                    // Fast path: try to parse header in one go
                    let b0 = buf[0];
                    let b1 = buf[1];

                    // Parse first byte
                    let fin = b0 & 0x80 != 0;
                    let rsv1 = b0 & 0x40 != 0;
                    let rsv2 = b0 & 0x20 != 0;
                    let rsv3 = b0 & 0x10 != 0;

                    // Check RSV bits (must be 0 unless extension negotiated)
                    // RSV1 is allowed when compression is enabled
                    if rsv1 && !self.allow_rsv1 {
                        return Err(Error::Protocol(
                            "RSV1 must be 0 (compression not negotiated)",
                        ));
                    }
                    if rsv2 || rsv3 {
                        return Err(Error::Protocol("RSV2 and RSV3 must be 0"));
                    }

                    let opcode =
                        OpCode::from_u8(b0 & 0x0F).ok_or(Error::InvalidFrame("invalid opcode"))?;

                    // Control frames must not be fragmented
                    if opcode.is_control() && !fin {
                        return Err(Error::Protocol("control frame must not be fragmented"));
                    }

                    // Parse second byte
                    let masked = b1 & 0x80 != 0;
                    let len_byte = b1 & 0x7F;

                    // Validate masking
                    if self.expect_masked && !masked {
                        return Err(Error::Protocol("client frames must be masked"));
                    }
                    if !self.expect_masked && masked {
                        return Err(Error::Protocol("server frames must not be masked"));
                    }

                    // Determine payload length
                    let (payload_len, header_size) = if len_byte <= 125 {
                        (len_byte as u64, 2)
                    } else if len_byte == 126 {
                        if buf.len() < 4 {
                            // Need more data for extended length
                            self.header_buf[0] = b0;
                            self.header_buf[1] = b1;
                            self.header_len = 2;
                            buf.advance(2); // Consume the 2 bytes we saved
                            self.state = ParseState::ExtendedLen16;
                            return Ok(None);
                        }
                        let len = u16::from_be_bytes([buf[2], buf[3]]) as u64;
                        // Validate minimum length for 16-bit encoding
                        if len < 126 {
                            return Err(Error::Protocol("payload length not minimal"));
                        }
                        (len, 4)
                    } else {
                        // len_byte == 127
                        if buf.len() < 10 {
                            self.header_buf[0] = b0;
                            self.header_buf[1] = b1;
                            self.header_len = 2;
                            buf.advance(2); // Consume the 2 bytes we saved
                            self.state = ParseState::ExtendedLen64;
                            return Ok(None);
                        }
                        let len = u64::from_be_bytes([
                            buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
                        ]);
                        // Validate minimum length for 64-bit encoding
                        if len <= 0xFFFF {
                            return Err(Error::Protocol("payload length not minimal"));
                        }
                        // Validate MSB is 0
                        if len >> 63 != 0 {
                            return Err(Error::Protocol("payload length MSB must be 0"));
                        }
                        (len, 10)
                    };

                    // Control frame length check
                    if opcode.is_control() && payload_len > 125 {
                        return Err(Error::Protocol("control frame too large"));
                    }

                    // Frame size check
                    if payload_len > self.max_frame_size as u64 {
                        return Err(Error::FrameTooLarge);
                    }

                    // Get mask if needed
                    let total_header = header_size + if masked { 4 } else { 0 };
                    if buf.len() < total_header {
                        // Save partial header and advance buffer
                        let to_copy = buf.len().min(14);
                        self.header_buf[..to_copy].copy_from_slice(&buf[..to_copy]);
                        self.header_len = to_copy;
                        buf.advance(to_copy);

                        // Create header without mask before transitioning to Mask state
                        self.header = Some(FrameHeader {
                            fin,
                            rsv1,
                            rsv2,
                            rsv3,
                            opcode,
                            masked,
                            payload_len,
                            mask: None,
                        });

                        self.state = ParseState::Mask;
                        return Ok(None);
                    }

                    let mask = if masked {
                        Some([
                            buf[header_size],
                            buf[header_size + 1],
                            buf[header_size + 2],
                            buf[header_size + 3],
                        ])
                    } else {
                        None
                    };

                    // We have the complete header
                    buf.advance(total_header);

                    self.header = Some(FrameHeader {
                        fin,
                        rsv1,
                        rsv2,
                        rsv3,
                        opcode,
                        masked,
                        payload_len,
                        mask,
                    });
                    self.state = ParseState::Payload;
                }

                ParseState::ExtendedLen16 => {
                    // Need bytes 2 and 3 for 16-bit length (total 4 bytes for header so far)
                    let target_len = 4;
                    let needed = target_len - self.header_len;
                    if buf.len() < needed {
                        // Copy what we have and advance buffer
                        let to_copy = buf.len();
                        self.header_buf[self.header_len..self.header_len + to_copy]
                            .copy_from_slice(&buf[..to_copy]);
                        self.header_len += to_copy;
                        buf.advance(to_copy);
                        return Ok(None);
                    }

                    // Copy remaining length bytes
                    self.header_buf[self.header_len..target_len].copy_from_slice(&buf[..needed]);
                    buf.advance(needed);
                    self.header_len = target_len;

                    let payload_len =
                        u16::from_be_bytes([self.header_buf[2], self.header_buf[3]]) as u64;

                    if payload_len < 126 {
                        return Err(Error::Protocol("payload length not minimal"));
                    }

                    // Continue to mask parsing
                    self.parse_header_with_len(payload_len)?;

                    if self.header.as_ref().unwrap().masked {
                        self.state = ParseState::Mask;
                    } else {
                        self.state = ParseState::Payload;
                    }
                }

                ParseState::ExtendedLen64 => {
                    // Need bytes 2-9 for 64-bit length (total 10 bytes for header so far)
                    let target_len = 10;
                    let needed = target_len - self.header_len;
                    if buf.len() < needed {
                        // Copy what we have and advance buffer
                        let to_copy = buf.len();
                        self.header_buf[self.header_len..self.header_len + to_copy]
                            .copy_from_slice(&buf[..to_copy]);
                        self.header_len += to_copy;
                        buf.advance(to_copy);
                        return Ok(None);
                    }

                    self.header_buf[self.header_len..target_len].copy_from_slice(&buf[..needed]);
                    buf.advance(needed);
                    self.header_len = target_len;

                    let payload_len = u64::from_be_bytes([
                        self.header_buf[2],
                        self.header_buf[3],
                        self.header_buf[4],
                        self.header_buf[5],
                        self.header_buf[6],
                        self.header_buf[7],
                        self.header_buf[8],
                        self.header_buf[9],
                    ]);

                    if payload_len <= 0xFFFF {
                        return Err(Error::Protocol("payload length not minimal"));
                    }
                    if payload_len >> 63 != 0 {
                        return Err(Error::Protocol("payload length MSB must be 0"));
                    }

                    self.parse_header_with_len(payload_len)?;

                    if self.header.as_ref().unwrap().masked {
                        self.state = ParseState::Mask;
                    } else {
                        self.state = ParseState::Payload;
                    }
                }

                ParseState::Mask => {
                    let header = self.header.as_mut().unwrap();
                    // Calculate base header size (before mask)
                    let header_base = if header.payload_len > MEDIUM_MESSAGE_THRESHOLD as u64 {
                        10 // 2 + 8 bytes for 64-bit length
                    } else if header.payload_len > SMALL_MESSAGE_THRESHOLD as u64 {
                        4 // 2 + 2 bytes for 16-bit length
                    } else {
                        2 // just the 2 base bytes
                    };
                    // Total header with mask is header_base + 4
                    let target_len = header_base + 4;
                    let needed = target_len - self.header_len;

                    if DEBUG {
                        eprintln!(
                            "[PARSER] Mask state: header_base={}, target_len={}, header_len={}, needed={}, payload_len={}",
                            header_base, target_len, self.header_len, needed, header.payload_len
                        );
                        eprintln!(
                            "[PARSER] header_buf so far: {:?}",
                            &self.header_buf[..self.header_len]
                        );
                    }

                    if buf.len() < needed {
                        // Copy what we have and advance buffer
                        let to_copy = buf.len();
                        self.header_buf[self.header_len..self.header_len + to_copy]
                            .copy_from_slice(&buf[..to_copy]);
                        self.header_len += to_copy;
                        buf.advance(to_copy);
                        return Ok(None);
                    }

                    // Copy remaining bytes needed to complete the mask
                    self.header_buf[self.header_len..target_len].copy_from_slice(&buf[..needed]);
                    buf.advance(needed);
                    self.header_len = target_len;

                    // Extract mask from header_buf at the correct position
                    header.mask = Some([
                        self.header_buf[header_base],
                        self.header_buf[header_base + 1],
                        self.header_buf[header_base + 2],
                        self.header_buf[header_base + 3],
                    ]);

                    self.state = ParseState::Payload;
                }

                ParseState::Payload => {
                    let header = self.header.as_ref().unwrap();
                    let payload_len = header.payload_len as usize;

                    if DEBUG {
                        eprintln!(
                            "[PARSER] Payload state: need {} bytes, have {} bytes (opcode: {:?}, fin: {}, rsv1: {})",
                            payload_len,
                            buf.len(),
                            header.opcode,
                            header.fin,
                            header.rsv1
                        );
                        if buf.len() > 0 {
                            eprintln!(
                                "[PARSER] First 16 bytes of buffer: {:?}",
                                &buf[..buf.len().min(16)]
                            );
                        }
                    }

                    if buf.len() < payload_len {
                        if DEBUG {
                            eprintln!("[PARSER] Not enough payload data, waiting...");
                        }
                        return Ok(None);
                    }

                    if DEBUG {
                        eprintln!(
                            "[PARSER] Extracting payload of {} bytes, buf will have {} bytes remaining",
                            payload_len,
                            buf.len() - payload_len
                        );
                    }

                    // Extract and unmask payload
                    let mut payload = buf.split_to(payload_len);

                    if let Some(mask) = header.mask {
                        apply_mask(&mut payload, mask);
                    }

                    let frame = Frame {
                        header: self.header.take().unwrap(),
                        payload: payload.freeze(),
                    };

                    if DEBUG {
                        eprintln!(
                            "[PARSER] Frame complete! Resetting parser state. Buffer now has {} bytes",
                            buf.len()
                        );
                    }

                    self.reset();
                    return Ok(Some(frame));
                }
            }
        }
    }

    /// Parse header from saved buffer with known length
    fn parse_header_with_len(&mut self, payload_len: u64) -> Result<()> {
        let b0 = self.header_buf[0];
        let b1 = self.header_buf[1];

        let fin = b0 & 0x80 != 0;
        let rsv1 = b0 & 0x40 != 0;
        let rsv2 = b0 & 0x20 != 0;
        let rsv3 = b0 & 0x10 != 0;
        let opcode = OpCode::from_u8(b0 & 0x0F).ok_or(Error::InvalidFrame("invalid opcode"))?;
        let masked = b1 & 0x80 != 0;

        if opcode.is_control() && payload_len > 125 {
            return Err(Error::Protocol("control frame too large"));
        }

        if payload_len > self.max_frame_size as u64 {
            return Err(Error::FrameTooLarge);
        }

        self.header = Some(FrameHeader {
            fin,
            rsv1,
            rsv2,
            rsv3,
            opcode,
            masked,
            payload_len,
            mask: None,
        });

        Ok(())
    }
}

/// Encode a frame into a buffer
///
/// This is the fast path for frame encoding. For masked frames (client mode),
/// the payload will be copied and masked.
#[inline]
pub fn encode_frame(
    buf: &mut BytesMut,
    opcode: OpCode,
    payload: &[u8],
    fin: bool,
    mask: Option<[u8; 4]>,
) {
    encode_frame_with_rsv(buf, opcode, payload, fin, mask, false)
}

/// Encode a frame with RSV1 bit control (for compression)
///
/// When `rsv1` is true, sets the RSV1 bit indicating compressed data.
pub fn encode_frame_with_rsv(
    buf: &mut BytesMut,
    opcode: OpCode,
    payload: &[u8],
    fin: bool,
    mask: Option<[u8; 4]>,
    rsv1: bool,
) {
    let payload_len = payload.len();

    // Reserve space for header + payload
    let header_size =
        2 + if payload_len > MEDIUM_MESSAGE_THRESHOLD {
            8
        } else if payload_len > SMALL_MESSAGE_THRESHOLD {
            2
        } else {
            0
        } + if mask.is_some() { 4 } else { 0 };

    buf.reserve(header_size + payload_len);

    // First byte: FIN + RSV1 + opcode
    let mut b0 = opcode as u8;
    if fin {
        b0 |= 0x80;
    }
    if rsv1 {
        b0 |= 0x40;
    }
    buf.put_u8(b0);

    // Second byte: mask flag + length
    let mask_bit = if mask.is_some() { 0x80 } else { 0x00 };

    if payload_len <= SMALL_MESSAGE_THRESHOLD {
        buf.put_u8(mask_bit | payload_len as u8);
    } else if payload_len <= MEDIUM_MESSAGE_THRESHOLD {
        buf.put_u8(mask_bit | 126);
        buf.put_u16(payload_len as u16);
    } else {
        buf.put_u8(mask_bit | 127);
        buf.put_u64(payload_len as u64);
    }

    // Mask
    if let Some(m) = mask {
        buf.put_slice(&m);

        // Copy and mask payload
        let start = buf.len();
        buf.put_slice(payload);
        apply_mask(&mut buf[start..], m);
    } else {
        buf.put_slice(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opcode() {
        assert!(OpCode::Ping.is_control());
        assert!(OpCode::Pong.is_control());
        assert!(OpCode::Close.is_control());
        assert!(!OpCode::Text.is_control());
        assert!(!OpCode::Binary.is_control());
        assert!(OpCode::Text.is_data());
        assert!(OpCode::Binary.is_data());
        assert!(OpCode::Continuation.is_data());
    }

    #[test]
    fn test_parse_small_unmasked() {
        let mut parser = FrameParser::new(1024 * 1024, false);
        let mut buf = BytesMut::from(&[0x81, 0x05, b'h', b'e', b'l', b'l', b'o'][..]);

        let frame = parser.parse(&mut buf).unwrap().unwrap();
        assert!(frame.header.fin);
        assert_eq!(frame.header.opcode, OpCode::Text);
        assert_eq!(frame.payload.as_ref(), b"hello");
    }

    #[test]
    fn test_parse_small_masked() {
        let mut parser = FrameParser::new(1024 * 1024, true);
        let mask = [0x37, 0xfa, 0x21, 0x3d];

        // "Hello" masked with the above key
        let mut payload = *b"Hello";
        apply_mask(&mut payload, mask);

        let mut buf = BytesMut::new();
        buf.put_u8(0x81); // FIN + Text
        buf.put_u8(0x85); // Masked + length 5
        buf.put_slice(&mask);
        buf.put_slice(&payload);

        let frame = parser.parse(&mut buf).unwrap().unwrap();
        assert_eq!(frame.payload.as_ref(), b"Hello");
    }

    #[test]
    fn test_parse_medium_length() {
        let mut parser = FrameParser::new(1024 * 1024, false);
        let payload = vec![0x42u8; 200];

        let mut buf = BytesMut::new();
        buf.put_u8(0x82); // FIN + Binary
        buf.put_u8(126); // Extended length marker
        buf.put_u16(200); // Actual length
        buf.put_slice(&payload);

        let frame = parser.parse(&mut buf).unwrap().unwrap();
        assert_eq!(frame.header.opcode, OpCode::Binary);
        assert_eq!(frame.payload.len(), 200);
    }

    #[test]
    fn test_encode_frame() {
        let mut buf = BytesMut::new();
        encode_frame(&mut buf, OpCode::Text, b"hello", true, None);

        assert_eq!(buf[0], 0x81); // FIN + Text
        assert_eq!(buf[1], 0x05); // Length 5
        assert_eq!(&buf[2..], b"hello");
    }

    #[test]
    fn test_encode_frame_masked() {
        let mask = [0x01, 0x02, 0x03, 0x04];
        let mut buf = BytesMut::new();
        encode_frame(&mut buf, OpCode::Text, b"test", true, Some(mask));

        assert_eq!(buf[0], 0x81); // FIN + Text
        assert_eq!(buf[1], 0x84); // Masked + Length 4
        assert_eq!(&buf[2..6], &mask);

        // Unmask and verify
        let mut payload = buf[6..].to_vec();
        apply_mask(&mut payload, mask);
        assert_eq!(&payload, b"test");
    }

    #[test]
    fn test_control_frame_fragmentation() {
        let mut parser = FrameParser::new(1024, false);
        let mut buf = BytesMut::from(&[0x09, 0x00][..]); // Ping, FIN=0 (invalid)
        buf[0] = 0x09; // Ping without FIN

        let result = parser.parse(&mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn test_close_frame() {
        let frame = Frame::close(1000, "goodbye");
        assert_eq!(frame.header.opcode, OpCode::Close);

        let close = frame.parse_close().unwrap();
        assert_eq!(close.code, 1000);
        assert_eq!(close.reason, "goodbye");
    }
}
