//! Per-Message Deflate Extension (RFC 7692)
//!
//! This module implements the permessage-deflate WebSocket extension,
//! which compresses message payloads using the DEFLATE algorithm.

use bytes::{Bytes, BytesMut};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::error::{Error, Result};

/// Trailer bytes that must be removed after compression and added before decompression
const DEFLATE_TRAILER: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// Default LZ77 window size (32KB = 2^15)
pub const DEFAULT_WINDOW_BITS: u8 = 15;

/// Minimum LZ77 window size (256 bytes = 2^8)
pub const MIN_WINDOW_BITS: u8 = 8;

/// Maximum LZ77 window size (32KB = 2^15)
pub const MAX_WINDOW_BITS: u8 = 15;

/// Configuration for permessage-deflate extension
#[derive(Debug, Clone)]
pub struct DeflateConfig {
    /// Server's maximum LZ77 window bits (for compression when server, decompression when client)
    pub server_max_window_bits: u8,
    /// Client's maximum LZ77 window bits (for compression when client, decompression when server)
    pub client_max_window_bits: u8,
    /// If true, server must reset compression context after each message
    pub server_no_context_takeover: bool,
    /// If true, client must reset compression context after each message
    pub client_no_context_takeover: bool,
    /// Compression level (0-9, where 0 is no compression, 9 is max)
    pub compression_level: u32,
    /// Minimum message size to compress (smaller messages may not benefit)
    pub compression_threshold: usize,
}

impl Default for DeflateConfig {
    fn default() -> Self {
        Self {
            server_max_window_bits: DEFAULT_WINDOW_BITS,
            client_max_window_bits: DEFAULT_WINDOW_BITS,
            server_no_context_takeover: false,
            client_no_context_takeover: false,
            compression_level: 6,      // Default zlib compression level
            compression_threshold: 32, // Don't compress tiny messages
        }
    }
}

impl DeflateConfig {
    /// Create config optimized for low memory usage
    pub fn low_memory() -> Self {
        Self {
            server_max_window_bits: 10, // 1KB window
            client_max_window_bits: 10,
            server_no_context_takeover: true,
            client_no_context_takeover: true,
            compression_level: 1, // Fast compression
            compression_threshold: 64,
        }
    }

    /// Create config optimized for best compression
    pub fn best_compression() -> Self {
        Self {
            server_max_window_bits: MAX_WINDOW_BITS,
            client_max_window_bits: MAX_WINDOW_BITS,
            server_no_context_takeover: false,
            client_no_context_takeover: false,
            compression_level: 9,
            compression_threshold: 16,
        }
    }

    /// Parse extension parameters from handshake
    pub fn from_params(params: &[(&str, Option<&str>)]) -> Result<Self> {
        let mut config = Self::default();

        for (name, value) in params {
            match *name {
                "server_no_context_takeover" => {
                    if value.is_some() {
                        return Err(Error::HandshakeFailed(
                            "server_no_context_takeover must not have a value",
                        ));
                    }
                    config.server_no_context_takeover = true;
                }
                "client_no_context_takeover" => {
                    if value.is_some() {
                        return Err(Error::HandshakeFailed(
                            "client_no_context_takeover must not have a value",
                        ));
                    }
                    config.client_no_context_takeover = true;
                    // When client uses no_context_takeover, server should too
                    // to ensure decompression works correctly on client side
                    config.server_no_context_takeover = true;
                }
                "server_max_window_bits" => {
                    if let Some(v) = value {
                        let bits: u8 = v.parse().map_err(|_| {
                            Error::HandshakeFailed("invalid server_max_window_bits value")
                        })?;
                        if !(MIN_WINDOW_BITS..=MAX_WINDOW_BITS).contains(&bits) {
                            return Err(Error::HandshakeFailed(
                                "server_max_window_bits out of range (8-15)",
                            ));
                        }
                        config.server_max_window_bits = bits;
                    }
                }
                "client_max_window_bits" => {
                    if let Some(v) = value {
                        let bits: u8 = v.parse().map_err(|_| {
                            Error::HandshakeFailed("invalid client_max_window_bits value")
                        })?;
                        if !(MIN_WINDOW_BITS..=MAX_WINDOW_BITS).contains(&bits) {
                            return Err(Error::HandshakeFailed(
                                "client_max_window_bits out of range (8-15)",
                            ));
                        }
                        config.client_max_window_bits = bits;
                    }
                    // If no value, client just indicates support
                }
                _ => {
                    return Err(Error::HandshakeFailed(
                        "unknown permessage-deflate parameter",
                    ));
                }
            }
        }

        Ok(config)
    }

    /// Generate extension response header value for server
    pub fn to_response_header(&self) -> String {
        let mut parts = vec!["permessage-deflate".to_string()];

        if self.server_no_context_takeover {
            parts.push("server_no_context_takeover".to_string());
        }
        if self.client_no_context_takeover {
            parts.push("client_no_context_takeover".to_string());
        }
        if self.server_max_window_bits < MAX_WINDOW_BITS {
            parts.push(format!(
                "server_max_window_bits={}",
                self.server_max_window_bits
            ));
        }
        if self.client_max_window_bits < MAX_WINDOW_BITS {
            parts.push(format!(
                "client_max_window_bits={}",
                self.client_max_window_bits
            ));
        }

        parts.join("; ")
    }
}

/// Deflate compressor for outgoing messages
pub struct DeflateEncoder {
    compress: Compress,
    no_context_takeover: bool,
    #[allow(dead_code)]
    window_bits: u8,
    #[allow(dead_code)]
    compression_level: Compression,
    threshold: usize,
}

impl DeflateEncoder {
    /// Create a new encoder
    pub fn new(window_bits: u8, no_context_takeover: bool, level: u32, threshold: usize) -> Self {
        let compression_level = Compression::new(level);
        // Use the negotiated window_bits for compression
        // This ensures the compressed data can be decompressed by clients with smaller windows
        let compress = Compress::new_with_window_bits(compression_level, false, window_bits);

        Self {
            compress,
            no_context_takeover,
            window_bits,
            compression_level,
            threshold,
        }
    }

    /// Compress a message payload
    ///
    /// Returns None if the message is too small to benefit from compression
    /// or if compression would make it larger.
    pub fn compress(&mut self, data: &[u8]) -> Result<Option<Bytes>> {
        if data.len() < self.threshold {
            return Ok(None);
        }

        // Reset context if required
        if self.no_context_takeover {
            self.compress.reset();
        }

        // Estimate output size (compressed data is often smaller, but we need headroom)
        let max_output = data.len() + 64;
        let mut output = BytesMut::with_capacity(max_output);

        // Compress the data
        let mut total_in: usize = 0;
        let mut iterations = 0u32;

        loop {
            iterations += 1;
            if iterations > 100_000 {
                return Err(Error::Compression(
                    "compression took too many iterations".into(),
                ));
            }

            // Ensure we have space in output buffer
            let available = output.capacity() - output.len();
            if available == 0 {
                output.reserve(4096);
            }

            let input = &data[total_in..];
            let before_out = self.compress.total_out();
            let before_in = self.compress.total_in();

            // Get writable slice using spare_capacity_mut to avoid UB with uninitialized memory.
            // We get the spare capacity, compress into it, then only set_len for bytes actually written.
            let out_start = output.len();
            let spare = output.spare_capacity_mut();

            // SAFETY: We're creating a &mut [u8] from MaybeUninit<u8> slice.
            // flate2's compress() will write to this buffer and tell us how many bytes were written.
            // We only call set_len() for the bytes that were actually initialized by compress().
            let spare_slice = unsafe {
                std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, spare.len())
            };

            let status = self
                .compress
                .compress(input, spare_slice, FlushCompress::Sync)
                .map_err(|e| Error::Compression(format!("deflate error: {}", e)))?;

            let consumed = (self.compress.total_in() - before_in) as usize;
            let produced = (self.compress.total_out() - before_out) as usize;

            total_in += consumed;

            // SAFETY: compress() wrote exactly `produced` bytes to spare_slice.
            // We're only extending the length by the number of bytes that were initialized.
            unsafe {
                output.set_len(out_start + produced);
            }

            match status {
                Status::Ok | Status::BufError => {
                    if total_in >= data.len() {
                        break;
                    }
                }
                Status::StreamEnd => break,
            }
        }

        // Per RFC 7692: Remove trailing 0x00 0x00 0xff 0xff
        if output.len() >= 4 && output.ends_with(&DEFLATE_TRAILER) {
            output.truncate(output.len() - 4);
        }

        // Only use compression if it actually reduces size
        if output.len() >= data.len() {
            return Ok(None);
        }

        Ok(Some(output.freeze()))
    }

    /// Reset the compression context (for no_context_takeover)
    pub fn reset(&mut self) {
        self.compress.reset();
    }
}

/// Deflate decompressor for incoming messages
pub struct DeflateDecoder {
    decompress: Decompress,
    no_context_takeover: bool,
    #[allow(dead_code)]
    window_bits: u8,
}

impl DeflateDecoder {
    /// Create a new decoder
    pub fn new(window_bits: u8, no_context_takeover: bool) -> Self {
        // Use raw deflate (no zlib header) with the negotiated window_bits
        let decompress = Decompress::new_with_window_bits(false, window_bits);

        Self {
            decompress,
            no_context_takeover,
            window_bits,
        }
    }

    /// Decompress a message payload
    pub fn decompress(&mut self, data: &[u8], max_size: usize) -> Result<Bytes> {
        // Reset context if required
        if self.no_context_takeover {
            self.decompress.reset(false);
        }

        // Per RFC 7692: Append 0x00 0x00 0xff 0xff before decompressing
        let mut input = BytesMut::with_capacity(data.len() + 4);
        input.extend_from_slice(data);
        input.extend_from_slice(&DEFLATE_TRAILER);

        // Start with reasonable output buffer (at least 1KB or 4x input)
        let initial_cap = std::cmp::max(1024, data.len() * 4);
        let mut output = BytesMut::with_capacity(initial_cap);
        let mut total_in: usize = 0;
        let mut iterations = 0u32;

        loop {
            iterations += 1;
            // Safety check to prevent infinite loops
            if iterations > 100_000 {
                return Err(Error::Compression(
                    "decompression took too many iterations".into(),
                ));
            }

            // Check size limit
            if output.len() > max_size {
                return Err(Error::MessageTooLarge);
            }

            // Ensure we have space in output buffer
            let available = output.capacity() - output.len();
            if available == 0 {
                if output.capacity() >= max_size {
                    return Err(Error::MessageTooLarge);
                }
                // At least double or add 4KB, whichever is larger
                let additional = std::cmp::max(output.capacity(), 4096);
                output.reserve(additional);
            }

            let before_out = self.decompress.total_out();
            let before_in = self.decompress.total_in();

            // Get writable slice using spare_capacity_mut to avoid UB with uninitialized memory.
            let out_start = output.len();
            let spare = output.spare_capacity_mut();

            // SAFETY: We're creating a &mut [u8] from MaybeUninit<u8> slice.
            // flate2's decompress() will write to this buffer and tell us how many bytes were written.
            // We only call set_len() for the bytes that were actually initialized by decompress().
            let spare_slice = unsafe {
                std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, spare.len())
            };

            let status = self
                .decompress
                .decompress(&input[total_in..], spare_slice, FlushDecompress::Sync)
                .map_err(|e| Error::Compression(format!("inflate error: {}", e)))?;

            let consumed = (self.decompress.total_in() - before_in) as usize;
            let produced = (self.decompress.total_out() - before_out) as usize;

            total_in += consumed;

            // SAFETY: decompress() wrote exactly `produced` bytes to spare_slice.
            // We're only extending the length by the number of bytes that were initialized.
            unsafe {
                output.set_len(out_start + produced);
            }

            match status {
                Status::Ok => {
                    if total_in >= input.len() {
                        break;
                    }
                }
                Status::StreamEnd => break,
                Status::BufError => {
                    // Need more output space - will be handled at top of loop
                }
            }
        }

        Ok(output.freeze())
    }

    /// Reset the decompression context (for no_context_takeover)
    pub fn reset(&mut self) {
        self.decompress.reset(false);
    }
}

/// Combined compressor/decompressor context for a WebSocket connection
pub struct DeflateContext {
    /// Encoder for outgoing messages
    pub encoder: DeflateEncoder,
    /// Decoder for incoming messages
    pub decoder: DeflateDecoder,
    /// Configuration
    pub config: DeflateConfig,
}

impl DeflateContext {
    /// Create context for server role
    pub fn server(config: DeflateConfig) -> Self {
        let encoder = DeflateEncoder::new(
            config.server_max_window_bits,
            config.server_no_context_takeover,
            config.compression_level,
            config.compression_threshold,
        );
        let decoder = DeflateDecoder::new(
            config.client_max_window_bits,
            config.client_no_context_takeover,
        );

        Self {
            encoder,
            decoder,
            config,
        }
    }

    /// Create context for client role
    pub fn client(config: DeflateConfig) -> Self {
        let encoder = DeflateEncoder::new(
            config.client_max_window_bits,
            config.client_no_context_takeover,
            config.compression_level,
            config.compression_threshold,
        );
        let decoder = DeflateDecoder::new(
            config.server_max_window_bits,
            config.server_no_context_takeover,
        );

        Self {
            encoder,
            decoder,
            config,
        }
    }

    /// Compress a message if beneficial
    pub fn compress(&mut self, data: &[u8]) -> Result<Option<Bytes>> {
        self.encoder.compress(data)
    }

    /// Decompress a message
    pub fn decompress(&mut self, data: &[u8], max_size: usize) -> Result<Bytes> {
        self.decoder.decompress(data, max_size)
    }
}

/// Parse permessage-deflate extension parameters from header value
pub fn parse_deflate_offer(value: &str) -> Option<Vec<(&str, Option<&str>)>> {
    let value = value.trim();

    // Check if this is a permessage-deflate offer
    if !value.starts_with("permessage-deflate") {
        return None;
    }

    let rest = value.strip_prefix("permessage-deflate")?.trim_start();

    if rest.is_empty() {
        return Some(Vec::new());
    }

    // Must start with semicolon if there are parameters
    if !rest.starts_with(';') {
        return None;
    }

    let mut params = Vec::new();

    for part in rest[1..].split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some((name, value)) = part.split_once('=') {
            let name = name.trim();
            let value = value.trim().trim_matches('"');
            params.push((name, Some(value)));
        } else {
            params.push((part, None));
        }
    }

    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_decompress() {
        let config = DeflateConfig::default();
        let mut ctx = DeflateContext::server(config);

        let original = b"Hello, World! This is a test message that should be compressed.";

        // Compress
        let compressed = ctx.compress(original).unwrap();
        assert!(compressed.is_some());
        let compressed = compressed.unwrap();
        assert!(compressed.len() < original.len());

        // Decompress
        let decompressed = ctx.decompress(&compressed, 1024).unwrap();
        assert_eq!(&decompressed[..], &original[..]);
    }

    #[test]
    fn test_small_message_not_compressed() {
        let config = DeflateConfig {
            compression_threshold: 100,
            ..Default::default()
        };
        let mut ctx = DeflateContext::server(config);

        let small = b"tiny";
        let result = ctx.compress(small).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_context_takeover() {
        let config = DeflateConfig {
            server_no_context_takeover: false,
            compression_threshold: 0,
            ..Default::default()
        };
        let mut ctx = DeflateContext::server(config);

        let msg = b"Hello, World! Hello, World! Hello, World!";

        // First compression
        let first = ctx.compress(msg).unwrap().unwrap();

        // Second compression should benefit from context
        let second = ctx.compress(msg).unwrap().unwrap();

        // With context takeover, second should be smaller or equal
        // (references previous data in LZ77 window)
        assert!(second.len() <= first.len());
    }

    #[test]
    fn test_no_context_takeover() {
        let config = DeflateConfig {
            server_no_context_takeover: true,
            compression_threshold: 0,
            ..Default::default()
        };
        let mut ctx = DeflateContext::server(config);

        let msg = b"Hello, World! Hello, World! Hello, World!";

        // Both compressions should produce same output
        let first = ctx.compress(msg).unwrap().unwrap();
        let second = ctx.compress(msg).unwrap().unwrap();

        assert_eq!(first.len(), second.len());
    }

    #[test]
    fn test_parse_deflate_offer() {
        // Simple offer
        let params = parse_deflate_offer("permessage-deflate").unwrap();
        assert!(params.is_empty());

        // With parameters
        let params = parse_deflate_offer(
            "permessage-deflate; server_no_context_takeover; server_max_window_bits=10",
        )
        .unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], ("server_no_context_takeover", None));
        assert_eq!(params[1], ("server_max_window_bits", Some("10")));

        // Not a deflate offer
        assert!(parse_deflate_offer("some-other-extension").is_none());
    }

    #[test]
    fn test_config_from_params() {
        let params = vec![
            ("server_no_context_takeover", None),
            ("client_max_window_bits", Some("12")),
        ];

        let config = DeflateConfig::from_params(&params).unwrap();
        assert!(config.server_no_context_takeover);
        assert!(!config.client_no_context_takeover);
        assert_eq!(config.client_max_window_bits, 12);
        assert_eq!(config.server_max_window_bits, DEFAULT_WINDOW_BITS);
    }

    #[test]
    fn test_response_header() {
        let config = DeflateConfig {
            server_no_context_takeover: true,
            server_max_window_bits: 12,
            ..Default::default()
        };

        let header = config.to_response_header();
        assert!(header.contains("permessage-deflate"));
        assert!(header.contains("server_no_context_takeover"));
        assert!(header.contains("server_max_window_bits=12"));
    }
}
