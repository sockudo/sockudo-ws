//! Native Autobahn WebSocket conformance engine.
#![deny(missing_docs)]
pub mod catalog;
pub mod codec;
pub mod compression;
pub mod config;
pub mod handshake;
pub mod report;
pub mod runner;
pub mod serializer;
pub mod service;

/// Errors produced by transport and protocol operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An underlying I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A peer violated the WebSocket protocol.
    #[error("WebSocket protocol error: {0}")]
    Protocol(String),
    /// A configured resource limit was exceeded.
    #[error("resource limit: {0}")]
    Limit(&'static str),
    /// A handshake was rejected or malformed.
    #[error("handshake: {0}")]
    Handshake(String),
    /// A configuration value is invalid.
    #[error("configuration: {0}")]
    Config(String),
    /// A deadline expired.
    #[error("operation timed out")]
    Timeout,
    /// Invalid compressed data.
    #[error("compression: {0}")]
    Compression(String),
}
/// Result type shared by the engine.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests;
