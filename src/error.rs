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
    /// The matching Pong for a native keepalive Ping was not received in time.
    HeartbeatTimeout,
    /// No valid inbound frame was received before the configured hard idle deadline.
    IdleTimeout,

    // ========================================================================
    // Transport-specific errors
    // ========================================================================
    /// HTTP/2 error
    #[cfg(feature = "http2")]
    Http2(h2::Error),

    /// HTTP/3 error
    #[cfg(feature = "http3")]
    Http3(String),

    /// QUIC connection error
    #[cfg(feature = "http3")]
    Quic(quinn::ConnectionError),

    /// QUIC write error
    #[cfg(feature = "http3")]
    QuicWrite(quinn::WriteError),

    /// Extended CONNECT protocol not supported by server
    ExtendedConnectNotSupported,

    /// Stream was reset by peer
    StreamReset,
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

// ============================================================================
// Error Categorization
// ============================================================================

/// Error category for classification and metrics
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// I/O and connection errors
    Io,
    /// WebSocket protocol violations
    Protocol,
    /// Handshake failures
    Handshake,
    /// Data validation errors (UTF-8, frame structure)
    Validation,
    /// Resource limits exceeded (frame/message size)
    Capacity,
    /// Compression/decompression errors
    Compression,
    /// Connection state errors
    Connection,
    /// Transport-specific errors (HTTP/2, HTTP/3, QUIC)
    Transport,
}

impl Error {
    /// Get the error category/kind
    ///
    /// Useful for metrics, logging, and error handling decisions.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Io(_) => ErrorKind::Io,
            Error::InvalidFrame(_) => ErrorKind::Validation,
            Error::InvalidUtf8 => ErrorKind::Validation,
            Error::Protocol(_) => ErrorKind::Protocol,
            Error::ConnectionClosed => ErrorKind::Connection,
            Error::MessageTooLarge => ErrorKind::Capacity,
            Error::FrameTooLarge => ErrorKind::Capacity,
            Error::InvalidHttp(_) => ErrorKind::Handshake,
            Error::HandshakeFailed(_) => ErrorKind::Handshake,
            Error::BufferFull => ErrorKind::Capacity,
            Error::WouldBlock => ErrorKind::Io,
            Error::ConnectionReset => ErrorKind::Connection,
            Error::InvalidState(_) => ErrorKind::Protocol,
            Error::Closed(_) => ErrorKind::Connection,
            Error::InvalidCloseCode(_) => ErrorKind::Validation,
            Error::Capacity(_) => ErrorKind::Capacity,
            Error::Compression(_) => ErrorKind::Compression,
            Error::HeartbeatTimeout | Error::IdleTimeout => ErrorKind::Connection,
            #[cfg(feature = "http2")]
            Error::Http2(_) => ErrorKind::Transport,
            #[cfg(feature = "http3")]
            Error::Http3(_) => ErrorKind::Transport,
            #[cfg(feature = "http3")]
            Error::Quic(_) => ErrorKind::Transport,
            #[cfg(feature = "http3")]
            Error::QuicWrite(_) => ErrorKind::Transport,
            Error::ExtendedConnectNotSupported => ErrorKind::Handshake,
            Error::StreamReset => ErrorKind::Connection,
        }
    }

    /// Check if this error is fatal (connection cannot continue)
    ///
    /// Fatal errors indicate the connection is broken and cannot be recovered.
    /// Non-fatal errors may allow the connection to continue after handling.
    #[inline]
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Error::ConnectionClosed
                | Error::ConnectionReset
                | Error::Protocol(_)
                | Error::InvalidUtf8
                | Error::FrameTooLarge
                | Error::MessageTooLarge
                | Error::StreamReset
                | Error::InvalidCloseCode(_)
                | Error::HeartbeatTimeout
                | Error::IdleTimeout
        )
    }

    /// Check if this error is recoverable
    ///
    /// Recoverable errors are transient and the operation may succeed if retried.
    #[inline]
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Error::WouldBlock | Error::BufferFull)
    }

    /// Check if this error is a timeout
    ///
    /// Note: Timeout errors are typically wrapped by the caller using
    /// `tokio::time::timeout`, but some transport errors may indicate timeouts.
    #[inline]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::HeartbeatTimeout | Error::IdleTimeout)
            || matches!(self, Error::Io(e) if e.kind() == io::ErrorKind::TimedOut)
    }

    /// Check if this error is a connection error
    #[inline]
    pub fn is_connection_error(&self) -> bool {
        self.kind() == ErrorKind::Connection
    }

    /// Check if this error is a protocol error
    #[inline]
    pub fn is_protocol_error(&self) -> bool {
        self.kind() == ErrorKind::Protocol
    }

    /// Get a metric-friendly name for this error
    ///
    /// Returns a short, lowercase, underscore-separated string suitable
    /// for use in metrics labels.
    pub fn metric_name(&self) -> &'static str {
        match self {
            Error::Io(_) => "io_error",
            Error::InvalidFrame(_) => "invalid_frame",
            Error::InvalidUtf8 => "invalid_utf8",
            Error::Protocol(_) => "protocol_error",
            Error::ConnectionClosed => "connection_closed",
            Error::MessageTooLarge => "message_too_large",
            Error::FrameTooLarge => "frame_too_large",
            Error::InvalidHttp(_) => "invalid_http",
            Error::HandshakeFailed(_) => "handshake_failed",
            Error::BufferFull => "buffer_full",
            Error::WouldBlock => "would_block",
            Error::ConnectionReset => "connection_reset",
            Error::InvalidState(_) => "invalid_state",
            Error::Closed(_) => "closed",
            Error::InvalidCloseCode(_) => "invalid_close_code",
            Error::Capacity(_) => "capacity_exceeded",
            Error::Compression(_) => "compression_error",
            Error::HeartbeatTimeout => "heartbeat_timeout",
            Error::IdleTimeout => "idle_timeout",
            #[cfg(feature = "http2")]
            Error::Http2(_) => "http2_error",
            #[cfg(feature = "http3")]
            Error::Http3(_) => "http3_error",
            #[cfg(feature = "http3")]
            Error::Quic(_) => "quic_error",
            #[cfg(feature = "http3")]
            Error::QuicWrite(_) => "quic_write_error",
            Error::ExtendedConnectNotSupported => "extended_connect_not_supported",
            Error::StreamReset => "stream_reset",
        }
    }

    /// Get a suggested HTTP status code for this error
    ///
    /// Useful when converting WebSocket errors to HTTP responses
    /// during handshake or in HTTP/2/HTTP/3 contexts.
    pub fn suggested_http_status(&self) -> u16 {
        match self {
            Error::InvalidHttp(_) | Error::InvalidFrame(_) | Error::InvalidCloseCode(_) => 400, // Bad Request
            Error::Protocol(_) | Error::InvalidUtf8 => 400, // Bad Request
            Error::HandshakeFailed(_) => 400,               // Bad Request
            Error::MessageTooLarge | Error::FrameTooLarge | Error::Capacity(_) => 413, // Payload Too Large
            Error::BufferFull => 503, // Service Unavailable
            Error::ExtendedConnectNotSupported => 501, // Not Implemented
            _ => 500,                 // Internal Server Error
        }
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
            Error::HeartbeatTimeout => write!(f, "WebSocket Pong deadline expired"),
            Error::IdleTimeout => write!(f, "WebSocket inbound idle deadline expired"),
            #[cfg(feature = "http2")]
            Error::Http2(e) => write!(f, "HTTP/2 error: {}", e),
            #[cfg(feature = "http3")]
            Error::Http3(msg) => write!(f, "HTTP/3 error: {}", msg),
            #[cfg(feature = "http3")]
            Error::Quic(e) => write!(f, "QUIC error: {}", e),
            #[cfg(feature = "http3")]
            Error::QuicWrite(e) => write!(f, "QUIC write error: {}", e),
            Error::ExtendedConnectNotSupported => {
                write!(f, "Extended CONNECT protocol not supported by server")
            }
            Error::StreamReset => write!(f, "Stream was reset by peer"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            #[cfg(feature = "http2")]
            Error::Http2(e) => Some(e),
            #[cfg(feature = "http3")]
            Error::Quic(e) => Some(e),
            #[cfg(feature = "http3")]
            Error::QuicWrite(e) => Some(e),
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
            other => io::Error::other(other.to_string()),
        }
    }
}

// ============================================================================
// Transport-specific From implementations
// ============================================================================

#[cfg(feature = "http2")]
impl From<h2::Error> for Error {
    fn from(e: h2::Error) -> Self {
        if e.is_io() {
            // Clone the error info before consuming it
            let err_string = e.to_string();
            if let Some(io_err) = e.into_io() {
                return Error::Io(io_err);
            }
            // If into_io returned None despite is_io being true, wrap as generic IO error
            return Error::Io(std::io::Error::other(err_string));
        }
        Error::Http2(e)
    }
}

#[cfg(feature = "http3")]
impl From<quinn::ConnectionError> for Error {
    fn from(e: quinn::ConnectionError) -> Self {
        Error::Quic(e)
    }
}

#[cfg(feature = "http3")]
impl From<quinn::WriteError> for Error {
    fn from(e: quinn::WriteError) -> Self {
        Error::QuicWrite(e)
    }
}

#[cfg(feature = "http3")]
impl From<h3::error::ConnectionError> for Error {
    fn from(e: h3::error::ConnectionError) -> Self {
        Error::Http3(e.to_string())
    }
}

#[cfg(feature = "http3")]
impl From<h3::error::StreamError> for Error {
    fn from(e: h3::error::StreamError) -> Self {
        Error::Http3(e.to_string())
    }
}
