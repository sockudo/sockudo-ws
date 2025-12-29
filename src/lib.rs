//! # Sockudo-WS: Ultra-low latency WebSocket library
//!
//! A high-performance WebSocket library designed for HFT applications,
//! fully compatible with Tokio and Axum.
//!
//! ## Performance Features
//!
//! - **SIMD Acceleration**: AVX2/AVX-512/NEON for frame masking and UTF-8 validation
//! - **Zero-Copy Parsing**: Direct buffer access without intermediate copies
//! - **Custom Memory Pools**: No allocations in the hot path
//! - **Write Batching (Corking)**: Minimizes syscalls via vectored I/O
//! - **Cache-Line Alignment**: Prevents false sharing in concurrent scenarios
//! - **Lock-Free Queues**: SPSC/MPMC for cross-task communication
//!
//! ## Example with Axum
//!
//! ```ignore
//! use axum::{Router, routing::get};
//! use sockudo_ws::axum::WebSocketUpgrade;
//!
//! async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
//!     ws.on_upgrade(|socket| async move {
//!         // Handle WebSocket connection
//!     })
//! }
//!
//! let app = Router::new().route("/ws", get(ws_handler));
//! ```

#![allow(dead_code)]
#![allow(clippy::missing_safety_doc)]

pub mod alloc;
pub mod cork;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod mask;
pub mod protocol;
pub mod queue;
pub mod simd;
pub mod stream;
pub mod utf8;

#[cfg(feature = "permessage-deflate")]
pub mod deflate;

#[cfg(feature = "axum-integration")]
pub mod axum_integration;

pub use error::{Error, Result};
pub use frame::{Frame, OpCode};
pub use protocol::{Message, Role};
pub use stream::{ReuniteError, SplitReader, SplitWriter, WebSocketStream, reunite};

// Re-export config types at top level for convenience

#[cfg(feature = "permessage-deflate")]
pub use deflate::{DeflateConfig, DeflateContext};
#[cfg(feature = "permessage-deflate")]
pub use protocol::CompressedProtocol;

/// Cache line size for modern CPUs (64 bytes for x86_64, ARM64)
pub const CACHE_LINE_SIZE: usize = 64;

/// Default cork buffer size (16KB like uWebSockets)
pub const CORK_BUFFER_SIZE: usize = 16 * 1024;

/// Default receive buffer size (64KB for high throughput)
pub const RECV_BUFFER_SIZE: usize = 64 * 1024;

/// Maximum WebSocket frame header size (2 + 8 + 4 = 14 bytes)
pub const MAX_FRAME_HEADER_SIZE: usize = 14;

/// Small message threshold for fast-path optimization (< 126 bytes uses 2-byte header)
pub const SMALL_MESSAGE_THRESHOLD: usize = 125;

/// Medium message threshold (< 64KB uses 4-byte header)
pub const MEDIUM_MESSAGE_THRESHOLD: usize = 65535;

/// WebSocket GUID for handshake
pub const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Compression mode for WebSocket connections
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// No compression
    #[default]
    Disabled,
    /// Dedicated compressor per connection (more memory, better ratio)
    Dedicated,
    /// Shared compressor across connections (less memory, good for many connections)
    Shared,
    /// Shared compressor with 4KB sliding window
    Shared4KB,
    /// Shared compressor with 8KB sliding window
    Shared8KB,
    /// Shared compressor with 16KB sliding window
    Shared16KB,
}

impl Compression {
    /// Returns true if compression is enabled
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Compression::Disabled)
    }
}

/// Configuration for WebSocket connections
///
/// Mirrors uWebSockets configuration options for familiarity.
///
/// # Example
///
/// ```
/// use sockudo_ws::{Config, Compression};
///
/// let config = Config::builder()
///     .compression(Compression::Shared)
///     .max_payload_length(16 * 1024)
///     .idle_timeout(10)
///     .max_backpressure(1024 * 1024)
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum message size (default: 64MB)
    /// Equivalent to uWS maxPayloadLength
    pub max_message_size: usize,
    /// Maximum frame size (default: 16MB)
    pub max_frame_size: usize,
    /// Write buffer size for corking (default: 16KB)
    pub write_buffer_size: usize,
    /// Compression mode (default: Disabled)
    pub compression: Compression,
    /// Idle timeout in seconds (default: 120, 0 = disabled)
    /// Connection is closed if no data received within this time
    pub idle_timeout: u32,
    /// Maximum backpressure in bytes before dropping connection (default: 1MB)
    /// If write buffer exceeds this, connection is closed
    pub max_backpressure: usize,
    /// Send pings automatically to keep connection alive (default: true)
    pub auto_ping: bool,
    /// Ping interval in seconds (default: 30)
    pub ping_interval: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_message_size: 64 * 1024 * 1024,
            max_frame_size: 16 * 1024 * 1024,
            write_buffer_size: CORK_BUFFER_SIZE,
            compression: Compression::Disabled,
            idle_timeout: 120,
            max_backpressure: 1024 * 1024,
            auto_ping: true,
            ping_interval: 30,
        }
    }
}

impl Config {
    /// Create a new config builder
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::new()
    }

    /// Create config with uWebSockets-style defaults
    pub fn uws_defaults() -> Self {
        Self {
            max_message_size: 16 * 1024,
            max_frame_size: 16 * 1024,
            write_buffer_size: CORK_BUFFER_SIZE,
            compression: Compression::Shared,
            idle_timeout: 10,
            max_backpressure: 1024 * 1024,
            auto_ping: true,
            ping_interval: 30,
        }
    }
}

/// Builder for WebSocket configuration
#[derive(Debug, Clone)]
pub struct ConfigBuilder {
    config: Config,
}

impl ConfigBuilder {
    /// Create a new builder with default values
    pub fn new() -> Self {
        Self {
            config: Config::default(),
        }
    }

    /// Set compression mode
    pub fn compression(mut self, compression: Compression) -> Self {
        self.config.compression = compression;
        self
    }

    /// Set maximum payload/message length (uWS: maxPayloadLength)
    pub fn max_payload_length(mut self, size: usize) -> Self {
        self.config.max_message_size = size;
        self.config.max_frame_size = size;
        self
    }

    /// Set maximum message size
    pub fn max_message_size(mut self, size: usize) -> Self {
        self.config.max_message_size = size;
        self
    }

    /// Set maximum frame size
    pub fn max_frame_size(mut self, size: usize) -> Self {
        self.config.max_frame_size = size;
        self
    }

    /// Set idle timeout in seconds (uWS: idleTimeout)
    /// Set to 0 to disable
    pub fn idle_timeout(mut self, seconds: u32) -> Self {
        self.config.idle_timeout = seconds;
        self
    }

    /// Set maximum backpressure before dropping connection (uWS: maxBackpressure)
    pub fn max_backpressure(mut self, bytes: usize) -> Self {
        self.config.max_backpressure = bytes;
        self
    }

    /// Set write buffer size for corking
    pub fn write_buffer_size(mut self, size: usize) -> Self {
        self.config.write_buffer_size = size;
        self
    }

    /// Enable or disable auto ping
    pub fn auto_ping(mut self, enabled: bool) -> Self {
        self.config.auto_ping = enabled;
        self
    }

    /// Set ping interval in seconds
    pub fn ping_interval(mut self, seconds: u32) -> Self {
        self.config.ping_interval = seconds;
        self
    }

    /// Build the configuration
    pub fn build(self) -> Config {
        self.config
    }
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Prelude module for convenient imports
pub mod prelude {
    pub use crate::Config;
    pub use crate::error::{Error, Result};
    pub use crate::frame::{Frame, OpCode};
    pub use crate::protocol::{Message, Role};
    pub use crate::stream::WebSocketStream;
}
