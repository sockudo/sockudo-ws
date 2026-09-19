//! Per-Message Deflate Extension (RFC 7692)
//!
//! This module implements the permessage-deflate WebSocket extension,
//! which compresses message payloads using the DEFLATE algorithm.

use bytes::{Bytes, BytesMut};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

pub use crate::DeflateWindowBits;
use crate::error::{Error, Result};

/// Trailer bytes that must be removed after compression and added before decompression
const DEFLATE_TRAILER: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

const MIN_RFC_WINDOW_BITS: u8 = 8;
const MAX_RFC_WINDOW_BITS: u8 = 15;

/// Default LZ77 window size (32KB = 2^15).
pub const DEFAULT_WINDOW_BITS: DeflateWindowBits = DeflateWindowBits::Bits15;

/// Minimum LZ77 window size supported by the configured backend.
pub const MIN_WINDOW_BITS: DeflateWindowBits = DeflateWindowBits::Bits9;

/// Maximum LZ77 window size (32KB = 2^15).
pub const MAX_WINDOW_BITS: DeflateWindowBits = DeflateWindowBits::Bits15;

fn response_header(
    config: &DeflateConfig,
    server_max_window_bits: Option<u8>,
    client_max_window_bits: Option<u8>,
) -> String {
    let mut parts = vec!["permessage-deflate".to_string()];

    if config.server_no_context_takeover {
        parts.push("server_no_context_takeover".to_string());
    }
    if config.client_no_context_takeover {
        parts.push("client_no_context_takeover".to_string());
    }
    if let Some(bits) = server_max_window_bits {
        parts.push(format!("server_max_window_bits={bits}"));
    }
    if let Some(bits) = client_max_window_bits {
        parts.push(format!("client_max_window_bits={bits}"));
    }

    parts.join("; ")
}

/// Configuration for permessage-deflate extension
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeflateConfig {
    /// Server's maximum LZ77 window bits (for compression when server, decompression when client)
    pub server_max_window_bits: DeflateWindowBits,
    /// Client's maximum LZ77 window bits (for compression when client, decompression when server)
    pub client_max_window_bits: DeflateWindowBits,
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
    /// Create config optimized for low memory usage.
    ///
    /// This reduces encoder and retained takeover-history windows. The
    /// configured zlib-rs decoder may still retain its internal 32 KiB window.
    pub fn low_memory() -> Self {
        Self {
            server_max_window_bits: DeflateWindowBits::Bits10, // 1KB window
            client_max_window_bits: DeflateWindowBits::Bits10,
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
        let parsed = DeflateOffer::from_params(params).map_err(Error::HandshakeFailed)?;
        let mut config = Self {
            server_no_context_takeover: parsed.server_no_context_takeover,
            client_no_context_takeover: parsed.client_no_context_takeover,
            ..Self::default()
        };

        if let Some(bits) = parsed.server_max_window_bits {
            config.server_max_window_bits = DeflateWindowBits::try_from(bits)
                .map_err(|_| Error::HandshakeFailed("unsupported server_max_window_bits value"))?;
        }
        if let Some(Some(bits)) = parsed.client_max_window_bits {
            config.client_max_window_bits = if bits == MIN_RFC_WINDOW_BITS {
                DeflateWindowBits::Bits9
            } else {
                DeflateWindowBits::try_from(bits).map_err(|_| {
                    Error::HandshakeFailed("unsupported client_max_window_bits value")
                })?
            };
        }

        Ok(config)
    }

    /// Generate extension response header value for server
    pub fn to_response_header(&self) -> String {
        response_header(
            self,
            (self.server_max_window_bits < MAX_WINDOW_BITS)
                .then(|| self.server_max_window_bits.into()),
            (self.client_max_window_bits < MAX_WINDOW_BITS)
                .then(|| self.client_max_window_bits.into()),
        )
    }
}

/// Deflate compressor for outgoing messages
pub struct DeflateEncoder {
    compress: Compress,
    no_context_takeover: bool,
    #[allow(dead_code)]
    window_bits: DeflateWindowBits,
    #[allow(dead_code)]
    compression_level: Compression,
    threshold: usize,
}

impl DeflateEncoder {
    /// Create a new encoder
    pub fn new(
        window_bits: DeflateWindowBits,
        no_context_takeover: bool,
        level: u32,
        threshold: usize,
    ) -> Self {
        let compression_level = Compression::new(level);
        // Use the negotiated window_bits for compression
        // This ensures the compressed data can be decompressed by clients with smaller windows
        let compress = Compress::new_with_window_bits(compression_level, false, window_bits.into());

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

            let status = self
                .compress
                .compress_uninit(input, spare, FlushCompress::Sync)
                .map_err(|e| Error::Compression(format!("deflate error: {}", e)))?;

            let consumed = (self.compress.total_in() - before_in) as usize;
            let produced = (self.compress.total_out() - before_out) as usize;

            total_in += consumed;

            // SAFETY: compress_uninit() wrote exactly `produced` bytes to the spare capacity.
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

        // Only skip where the encoder resets per message. Otherwise the caller
        // sends these bytes raw, so they stay in our window without ever
        // entering the peer's, and every later back-reference resolves against
        // different history: corrupt messages, or a connection that dies.
        if self.no_context_takeover && output.len() >= data.len() {
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
    window_bits: DeflateWindowBits,
}

impl DeflateDecoder {
    /// Create a new decoder
    pub fn new(window_bits: DeflateWindowBits, no_context_takeover: bool) -> Self {
        // Use raw deflate (no zlib header) with the negotiated window_bits
        let decompress = Decompress::new_with_window_bits(false, window_bits.into());

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

            let status = self
                .decompress
                .decompress_uninit(&input[total_in..], spare, FlushDecompress::Sync)
                .map_err(|e| Error::Compression(format!("inflate error: {}", e)))?;

            let consumed = (self.decompress.total_in() - before_in) as usize;
            let produced = (self.decompress.total_out() - before_out) as usize;

            total_in += consumed;

            // SAFETY: decompress_uninit() wrote exactly `produced` bytes to the spare capacity.
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
    let mut parts = value.split(';');
    // Check that this is exactly a permessage-deflate offer.
    if parts.next()?.trim() != "permessage-deflate" {
        return None;
    }

    let mut params = Vec::new();
    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }

        if let Some((name, value)) = part.split_once('=') {
            let name = name.trim();
            let value = value.trim();
            let value = match (value.strip_prefix('"'), value.strip_suffix('"')) {
                (Some(value), Some(_)) => value.strip_suffix('"')?,
                (None, None) if !value.contains('"') => value,
                _ => return None,
            };
            params.push((name, Some(value)));
        } else {
            params.push((part, None));
        }
    }

    Some(params)
}

#[derive(Default)]
struct DeflateOffer {
    server_no_context_takeover: bool,
    client_no_context_takeover: bool,
    server_max_window_bits: Option<u8>,
    client_max_window_bits: Option<Option<u8>>,
}

/// Server-side result of negotiating a permessage-deflate offer.
///
/// The response parameters are kept separately from [`DeflateConfig`] because
/// RFC 7692 distinguishes an omitted parameter from an explicitly negotiated
/// value of 15, while both use the same backend codec configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeflateNegotiation {
    /// Runtime codec configuration selected for the connection.
    pub config: DeflateConfig,
    response_server_max_window_bits: Option<u8>,
    response_client_max_window_bits: Option<u8>,
}

impl DeflateNegotiation {
    /// Generate the extension response header value for this negotiation.
    pub fn to_response_header(&self) -> String {
        response_header(
            &self.config,
            self.response_server_max_window_bits,
            self.response_client_max_window_bits,
        )
    }
}

impl DeflateOffer {
    fn from_params(params: &[(&str, Option<&str>)]) -> std::result::Result<Self, &'static str> {
        let mut offer = Self::default();

        for &(name, value) in params {
            match name {
                "server_no_context_takeover" => {
                    if offer.server_no_context_takeover {
                        return Err("duplicate server_no_context_takeover");
                    }
                    if value.is_some() {
                        return Err("server_no_context_takeover must not have a value");
                    }
                    offer.server_no_context_takeover = true;
                }
                "client_no_context_takeover" => {
                    if offer.client_no_context_takeover {
                        return Err("duplicate client_no_context_takeover");
                    }
                    if value.is_some() {
                        return Err("client_no_context_takeover must not have a value");
                    }
                    offer.client_no_context_takeover = true;
                }
                "server_max_window_bits" => {
                    if offer.server_max_window_bits.is_some() {
                        return Err("duplicate server_max_window_bits");
                    }
                    let value = value.ok_or("server_max_window_bits requires a value")?;
                    let bits =
                        parse_window_bits(value).ok_or("invalid server_max_window_bits value")?;
                    offer.server_max_window_bits = Some(bits);
                }
                "client_max_window_bits" => {
                    if offer.client_max_window_bits.is_some() {
                        return Err("duplicate client_max_window_bits");
                    }
                    offer.client_max_window_bits = Some(match value {
                        Some(value) => Some(
                            parse_window_bits(value)
                                .ok_or("invalid client_max_window_bits value")?,
                        ),
                        // If no value, client just indicates support.
                        None => None,
                    });
                }
                _ => return Err("unknown permessage-deflate parameter"),
            }
        }

        Ok(offer)
    }
}

fn parse_window_bits(value: &str) -> Option<u8> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    let window_bits: u8 = value.parse().ok()?;
    if (MIN_RFC_WINDOW_BITS..=MAX_RFC_WINDOW_BITS).contains(&window_bits) {
        Some(window_bits)
    } else {
        None
    }
}

/// Select the first client offer that satisfies the server policy and backend limits.
pub fn negotiate_server_deflate(
    offers: &str,
    policy: &DeflateConfig,
) -> Option<DeflateNegotiation> {
    for offer in offers.split(',') {
        let Some(params) = parse_deflate_offer(offer.trim()) else {
            continue;
        };
        let Ok(offer) = DeflateOffer::from_params(&params) else {
            continue;
        };

        let (server_max_window_bits, response_server_max_window_bits) =
            match offer.server_max_window_bits {
                Some(limit) => {
                    let Ok(limit) = DeflateWindowBits::try_from(limit) else {
                        continue;
                    };
                    let selected = policy.server_max_window_bits.min(limit);
                    (selected, Some(selected.into()))
                }
                None => (
                    policy.server_max_window_bits,
                    (policy.server_max_window_bits < MAX_WINDOW_BITS)
                        .then(|| policy.server_max_window_bits.into()),
                ),
            };
        let (client_max_window_bits, response_client_max_window_bits) =
            match offer.client_max_window_bits {
                Some(Some(limit)) => {
                    let selected = u8::from(policy.client_max_window_bits).min(limit);
                    let backend = if selected == MIN_RFC_WINDOW_BITS {
                        MIN_WINDOW_BITS
                    } else {
                        DeflateWindowBits::try_from(selected).ok()?
                    };
                    (
                        backend,
                        (selected < u8::from(MAX_WINDOW_BITS)).then_some(selected),
                    )
                }
                Some(None) => (
                    policy.client_max_window_bits,
                    (policy.client_max_window_bits < MAX_WINDOW_BITS)
                        .then(|| policy.client_max_window_bits.into()),
                ),
                None if policy.client_max_window_bits == MAX_WINDOW_BITS => (MAX_WINDOW_BITS, None),
                None => continue,
            };

        let negotiated = DeflateConfig {
            server_max_window_bits,
            client_max_window_bits,
            server_no_context_takeover: policy.server_no_context_takeover
                || offer.server_no_context_takeover,
            client_no_context_takeover: policy.client_no_context_takeover
                || offer.client_no_context_takeover,
            compression_level: policy.compression_level,
            compression_threshold: policy.compression_threshold,
        };
        return Some(DeflateNegotiation {
            config: negotiated,
            response_server_max_window_bits,
            response_client_max_window_bits,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic high-entropy bytes, standing in for already-compressed
    /// payloads (audio, video, images) that deflate cannot shrink.
    fn incompressible(n: usize) -> Vec<u8> {
        let mut s: u64 = 0x2545_F491_4F6C_DD1D;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    /// One message as the protocol layer sends and receives it: a compressed
    /// result travels with RSV1 set and reaches the peer's inflater, while a
    /// `None` result is sent verbatim with RSV1 clear and never reaches it.
    fn round_trip(
        enc: &mut DeflateContext,
        dec: &mut DeflateContext,
        msg: &[u8],
    ) -> Result<Vec<u8>> {
        match enc.compress(msg)? {
            Some(compressed) => dec.decompress(&compressed, 1 << 20).map(|b| b.to_vec()),
            None => Ok(msg.to_vec()),
        }
    }

    #[test]
    fn test_undersized_compression_does_not_desync_context_takeover() {
        let config = DeflateConfig {
            server_no_context_takeover: false,
            client_no_context_takeover: false,
            compression_threshold: 16,
            ..Default::default()
        };
        let mut server = DeflateContext::server(config.clone());
        let mut client = DeflateContext::client(config);

        let text = b"{\"channel\":\"presence-room\",\"event\":\"client-typing\"}".repeat(8);

        // Warm both LZ77 windows with a message that does compress.
        let first = round_trip(&mut server, &mut client, &text).expect("first message");
        assert_eq!(first, text);

        // A message that does not shrink is sent verbatim, so the peer's window
        // never sees it. The encoder must not retain it either.
        let opaque = incompressible(4096);
        let second = round_trip(&mut server, &mut client, &opaque).expect("second message");
        assert_eq!(second, opaque);

        // Repeat the first message. With context takeover the encoder emits a
        // back-reference into its window; if that window still holds `opaque`,
        // the distance is wrong on the peer and the message decodes to garbage.
        let third = round_trip(&mut server, &mut client, &text).expect("third message decodes");
        assert_eq!(
            third, text,
            "window desynchronised after an uncompressed message"
        );
    }

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
        assert_eq!(config.client_max_window_bits, DeflateWindowBits::Bits12);
        assert_eq!(config.server_max_window_bits, DEFAULT_WINDOW_BITS);
    }

    #[test]
    fn config_from_params_keeps_context_takeover_directions_independent() {
        let config = DeflateConfig::from_params(&[("client_no_context_takeover", None)]).unwrap();

        assert!(!config.server_no_context_takeover);
        assert!(config.client_no_context_takeover);
    }

    #[test]
    fn config_from_params_rejects_duplicate_parameters() {
        for params in [
            vec![
                ("server_no_context_takeover", None),
                ("server_no_context_takeover", None),
            ],
            vec![
                ("client_no_context_takeover", None),
                ("client_no_context_takeover", None),
            ],
            vec![
                ("server_max_window_bits", Some("12")),
                ("server_max_window_bits", Some("11")),
            ],
            vec![
                ("client_max_window_bits", Some("12")),
                ("client_max_window_bits", Some("11")),
            ],
        ] {
            assert!(DeflateConfig::from_params(&params).is_err());
        }
    }

    #[test]
    fn config_from_params_rejects_valueless_server_window_and_unsupported_window() {
        assert!(DeflateConfig::from_params(&[("server_max_window_bits", None)]).is_err());
        assert!(DeflateConfig::from_params(&[("server_max_window_bits", Some("8"))]).is_err());
    }

    #[test]
    fn config_from_params_uses_a_supported_decoder_for_client_window_eight() {
        let config = DeflateConfig::from_params(&[("client_max_window_bits", Some("8"))]).unwrap();

        assert_eq!(config.client_max_window_bits, DeflateWindowBits::Bits9);
    }

    #[test]
    fn deflate_window_values_reject_leading_zeroes() {
        for parameter in ["server_max_window_bits", "client_max_window_bits"] {
            assert!(DeflateConfig::from_params(&[(parameter, Some("08"))]).is_err());
            assert!(
                negotiate_server_deflate(
                    &format!("permessage-deflate; {parameter}=08"),
                    &DeflateConfig::default(),
                )
                .is_none()
            );
        }
    }

    #[test]
    fn test_response_header() {
        let config = DeflateConfig {
            server_no_context_takeover: true,
            server_max_window_bits: DeflateWindowBits::Bits12,
            ..Default::default()
        };

        let header = config.to_response_header();
        assert!(header.contains("permessage-deflate"));
        assert!(header.contains("server_no_context_takeover"));
        assert!(header.contains("server_max_window_bits=12"));
    }

    #[test]
    fn server_negotiation_intersects_window_limits() {
        let policy = DeflateConfig {
            server_max_window_bits: DeflateWindowBits::Bits12,
            client_max_window_bits: DeflateWindowBits::Bits11,
            ..Default::default()
        };
        let negotiated = negotiate_server_deflate(
            "permessage-deflate; server_max_window_bits=10; client_max_window_bits=12",
            &policy,
        )
        .unwrap();

        assert_eq!(
            negotiated.config.server_max_window_bits,
            DeflateWindowBits::Bits10
        );
        assert_eq!(
            negotiated.config.client_max_window_bits,
            DeflateWindowBits::Bits11
        );
        assert_eq!(
            negotiated.to_response_header(),
            "permessage-deflate; server_max_window_bits=10; client_max_window_bits=11"
        );
    }

    #[test]
    fn server_negotiation_requires_client_window_parameter_for_a_smaller_policy() {
        let policy = DeflateConfig {
            client_max_window_bits: DeflateWindowBits::Bits10,
            ..Default::default()
        };

        assert!(negotiate_server_deflate("permessage-deflate", &policy).is_none());
        let negotiated =
            negotiate_server_deflate("permessage-deflate; client_max_window_bits", &policy)
                .unwrap();
        assert_eq!(
            negotiated.config.client_max_window_bits,
            DeflateWindowBits::Bits10
        );
    }

    #[test]
    fn server_negotiation_rejects_invalid_offers_and_selects_the_first_compatible_one() {
        let offers = concat!(
            "permessage-deflate; server_max_window_bits=8, ",
            "permessage-deflate; server_max_window_bits=12; server_max_window_bits=11, ",
            "other-extension, ",
            "permessage-deflate; server_max_window_bits=10"
        );
        let negotiated = negotiate_server_deflate(offers, &DeflateConfig::default()).unwrap();

        assert_eq!(
            negotiated.config.server_max_window_bits,
            DeflateWindowBits::Bits10
        );
    }

    #[test]
    fn server_negotiation_applies_context_takeover_constraints() {
        let negotiated = negotiate_server_deflate(
            "permessage-deflate; server_no_context_takeover; client_no_context_takeover",
            &DeflateConfig::default(),
        )
        .unwrap();

        assert!(negotiated.config.server_no_context_takeover);
        assert!(negotiated.config.client_no_context_takeover);
    }

    #[test]
    fn server_negotiation_does_not_emit_unoffered_client_window_parameter() {
        let negotiated =
            negotiate_server_deflate("permessage-deflate", &DeflateConfig::default()).unwrap();

        assert_eq!(negotiated.config.client_max_window_bits, MAX_WINDOW_BITS);
        assert!(
            !negotiated
                .to_response_header()
                .contains("client_max_window_bits")
        );
    }

    #[test]
    fn server_negotiation_preserves_an_offered_server_window_of_fifteen() {
        let negotiated = negotiate_server_deflate(
            "permessage-deflate; server_max_window_bits=15",
            &DeflateConfig::default(),
        )
        .unwrap();

        assert_eq!(
            negotiated.config.server_max_window_bits,
            DeflateWindowBits::Bits15
        );
        assert_eq!(
            negotiated.to_response_header(),
            "permessage-deflate; server_max_window_bits=15"
        );
    }

    #[test]
    fn server_negotiation_uses_a_supported_decoder_for_a_client_window_of_eight() {
        let negotiated = negotiate_server_deflate(
            "permessage-deflate; client_max_window_bits=8",
            &DeflateConfig::default(),
        )
        .unwrap();

        assert_eq!(
            negotiated.config.client_max_window_bits,
            DeflateWindowBits::Bits9
        );
        assert_eq!(
            negotiated.to_response_header(),
            "permessage-deflate; client_max_window_bits=8"
        );
    }

    #[test]
    fn server_negotiation_rejects_an_unsupported_server_window_of_eight() {
        assert!(
            negotiate_server_deflate(
                "permessage-deflate; server_max_window_bits=8",
                &DeflateConfig::default(),
            )
            .is_none()
        );
    }
}
