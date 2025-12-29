//! Error types for the WebSocket library

use std::fmt;
use std::io;

/// Result type alias for WebSocket operations
pub type Result<T> = std::result::Result<T, Error>;

/// WebSocket error types
#[derive(Debug)]
pub enum Error {
    /// I/O error from the underlying socket
    Io(io::Error),
    /// Invalid WebSocket frame
    InvalidFrame(&'static str),
    /// Invalid UTF-8 in text message
    InvalidUtf8,
    /// Protocol violation
    Protocol(&'static str),
    /// Connection closed normally
    ConnectionClosed,
    /// Message too large
    MessageTooLarge,
    /// Frame too large
    FrameTooLarge,
    /// Invalid HTTP request
    InvalidHttp(&'static str),
    /// Handshake failed
    HandshakeFailed(&'static str),
    /// Buffer full (backpressure)
    BufferFull,
    /// Would block (non-blocking I/O)
    WouldBlock,
    /// Connection reset by peer
    ConnectionReset,
    /// Invalid state
    InvalidState(&'static str),
    /// Close frame received
    Closed(Option<CloseReason>),
    /// Invalid close code
    InvalidCloseCode(u16),
    /// Capacity exceeded
    Capacity(&'static str),
    /// Compression/decompression error
    Compression(String),
}

/// Close frame reason
#[derive(Debug, Clone)]
pub struct CloseReason {
    /// Close status code
    pub code: u16,
    /// Optional reason string
    pub reason: String,
}

impl CloseReason {
    /// Normal closure
    pub const NORMAL: u16 = 1000;
    /// Going away (e.g., server shutdown)
    pub const GOING_AWAY: u16 = 1001;
    /// Protocol error
    pub const PROTOCOL_ERROR: u16 = 1002;
    /// Unsupported data
    pub const UNSUPPORTED: u16 = 1003;
    /// No status received
    pub const NO_STATUS: u16 = 1005;
    /// Abnormal closure
    pub const ABNORMAL: u16 = 1006;
    /// Invalid frame payload
    pub const INVALID_PAYLOAD: u16 = 1007;
    /// Policy violation
    pub const POLICY: u16 = 1008;
    /// Message too big
    pub const TOO_BIG: u16 = 1009;
    /// Mandatory extension
    pub const EXTENSION: u16 = 1010;
    /// Internal server error
    pub const INTERNAL: u16 = 1011;

    /// Create a new close reason
    pub fn new(code: u16, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }

    /// Check if the close code is valid per RFC 6455
    pub fn is_valid_code(code: u16) -> bool {
        matches!(code, 1000..=1003 | 1007..=1011 | 3000..=4999)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {}", e),
            Error::InvalidFrame(msg) => write!(f, "Invalid frame: {}", msg),
            Error::InvalidUtf8 => write!(f, "Invalid UTF-8 in text message"),
            Error::Protocol(msg) => write!(f, "Protocol error: {}", msg),
            Error::ConnectionClosed => write!(f, "Connection closed"),
            Error::MessageTooLarge => write!(f, "Message too large"),
            Error::FrameTooLarge => write!(f, "Frame too large"),
            Error::InvalidHttp(msg) => write!(f, "Invalid HTTP: {}", msg),
            Error::HandshakeFailed(msg) => write!(f, "Handshake failed: {}", msg),
            Error::BufferFull => write!(f, "Buffer full"),
            Error::WouldBlock => write!(f, "Would block"),
            Error::ConnectionReset => write!(f, "Connection reset by peer"),
            Error::InvalidState(msg) => write!(f, "Invalid state: {}", msg),
            Error::Closed(reason) => {
                if let Some(r) = reason {
                    write!(f, "Connection closed: {} ({})", r.code, r.reason)
                } else {
                    write!(f, "Connection closed")
                }
            }
            Error::InvalidCloseCode(code) => write!(f, "Invalid close code: {}", code),
            Error::Capacity(msg) => write!(f, "Capacity exceeded: {}", msg),
            Error::Compression(msg) => write!(f, "Compression error: {}", msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::WouldBlock => Error::WouldBlock,
            io::ErrorKind::ConnectionReset => Error::ConnectionReset,
            io::ErrorKind::BrokenPipe => Error::ConnectionClosed,
            io::ErrorKind::UnexpectedEof => Error::ConnectionClosed,
            _ => Error::Io(e),
        }
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Io(e) => e,
            Error::WouldBlock => io::Error::new(io::ErrorKind::WouldBlock, "would block"),
            Error::ConnectionReset => {
                io::Error::new(io::ErrorKind::ConnectionReset, "connection reset")
            }
            Error::ConnectionClosed => {
                io::Error::new(io::ErrorKind::BrokenPipe, "connection closed")
            }
            other => io::Error::new(io::ErrorKind::Other, other.to_string()),
        }
    }
}
