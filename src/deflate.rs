//! Per-Message Deflate Extension (RFC 7692)
//!
//! This module implements the permessage-deflate WebSocket extension,
//! which compresses message payloads using the DEFLATE algorithm.

use bytes::{Bytes, BytesMut};
use flate2::{Compress, Compression, FlushCompress, Status};
use libz_rs_sys::{
    Z_BUF_ERROR, Z_OK, Z_STREAM_END, Z_SYNC_FLUSH, inflate, inflateEnd, inflateInit2_,
    inflateReset, inflateResetKeep, z_stream, zlibVersion,
};
use std::mem::MaybeUninit;

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

struct RawDeflateDecoder {
    // Keep the stream at a stable address for the lifetime required by the C API.
    stream: Box<z_stream>,
}

// SAFETY: calls require exclusive access, borrowed buffers are cleared after
// each call, and the decoder owns the remaining zlib-rs state.
unsafe impl Send for RawDeflateDecoder {}
// SAFETY: shared access cannot call the mutable C API or expose its pointers.
unsafe impl Sync for RawDeflateDecoder {}

impl RawDeflateDecoder {
    fn new(window_bits: u8) -> Self {
        let mut stream = Box::new(z_stream::default());
        // A negative window size selects raw DEFLATE, as required by RFC 7692.
        // SAFETY: the default stream has valid allocator fields, remains at a
        // stable address, and is not used until initialization succeeds.
        let status = unsafe {
            inflateInit2_(
                &mut *stream,
                -i32::from(window_bits),
                zlibVersion(),
                std::mem::size_of::<z_stream>() as i32,
            )
        };
        assert_eq!(status, Z_OK, "failed to initialize DEFLATE decoder");
        Self { stream }
    }

    fn inflate(
        &mut self,
        input: &[u8],
        output: &mut [MaybeUninit<u8>],
    ) -> Result<(Status, usize, usize)> {
        let input = &input[..input.len().min(u32::MAX as usize)];
        let output_len = output.len().min(u32::MAX as usize);
        let output = &mut output[..output_len];
        self.stream.next_in = input.as_ptr();
        self.stream.avail_in = input.len() as u32;
        self.stream.next_out = output.as_mut_ptr().cast();
        self.stream.avail_out = output.len() as u32;

        // SAFETY: the stream is initialized, and its input and output pointers
        // refer to the slices above for the advertised lengths.
        let status = unsafe { inflate(&mut *self.stream, Z_SYNC_FLUSH) };
        let consumed = input.len() - self.stream.avail_in as usize;
        let produced = output.len() - self.stream.avail_out as usize;
        self.stream.next_in = std::ptr::null_mut();
        self.stream.avail_in = 0;
        self.stream.next_out = std::ptr::null_mut();
        self.stream.avail_out = 0;

        let status = match status {
            Z_OK => Status::Ok,
            Z_BUF_ERROR => Status::BufError,
            Z_STREAM_END => Status::StreamEnd,
            code => {
                return Err(Error::Compression(format!(
                    "inflate error: status code {code}"
                )));
            }
        };
        Ok((status, consumed, produced))
    }

    fn reset(&mut self, keep_window: bool) -> Result<()> {
        // SAFETY: the stream was initialized in new and is exclusively borrowed.
        let status = unsafe {
            if keep_window {
                inflateResetKeep(&mut *self.stream)
            } else {
                inflateReset(&mut *self.stream)
            }
        };
        if status == Z_OK {
            Ok(())
        } else {
            Err(Error::Compression(format!(
                "inflate reset error: status code {status}"
            )))
        }
    }
}

impl Drop for RawDeflateDecoder {
    fn drop(&mut self) {
        // SAFETY: the stream was initialized in new and is ended exactly once.
        let status = unsafe { inflateEnd(&mut *self.stream) };
        debug_assert_eq!(status, Z_OK);
    }
}

/// Deflate decompressor for incoming messages
pub struct DeflateDecoder {
    decompress: RawDeflateDecoder,
    no_context_takeover: bool,
}

impl DeflateDecoder {
    /// Create a new decoder
    pub fn new(window_bits: u8, no_context_takeover: bool) -> Self {
        // Use raw deflate (no zlib header) with the negotiated window_bits
        let decompress = RawDeflateDecoder::new(window_bits);

        Self {
            decompress,
            no_context_takeover,
        }
    }

    /// Decompress a message payload
    pub fn decompress(&mut self, data: &[u8], max_size: usize) -> Result<Bytes> {
        // Reset context if required
        if self.no_context_takeover {
            self.decompress.reset(false)?;
        }

        // Per RFC 7692: Append 0x00 0x00 0xff 0xff before decompressing
        let mut input = BytesMut::with_capacity(data.len() + 4);
        input.extend_from_slice(data);
        input.extend_from_slice(&DEFLATE_TRAILER);

        // Start with reasonable output buffer (at least 1KB or 4x input), but
        // never expose writable output beyond the configured message limit.
        let initial_cap = std::cmp::max(1024, data.len().saturating_mul(4)).min(max_size);
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

            let at_limit = output.len() == max_size;
            let offered;
            let status;
            let consumed;
            let produced;

            if at_limit {
                // Let inflate consume a trailer or report stream completion.
                // Any byte produced into this probe exceeds the logical limit.
                let mut probe = [std::mem::MaybeUninit::uninit()];
                offered = probe.len();
                (status, consumed, produced) =
                    self.decompress.inflate(&input[total_in..], &mut probe)?;
            } else {
                if output.len() == output.capacity() {
                    // At least double or add 4KB, whichever is larger. The
                    // allocator may reserve more, so the writable slice below
                    // is still capped explicitly.
                    let remaining = max_size - output.len();
                    let additional = std::cmp::max(output.capacity(), 4096).min(remaining);
                    output.reserve(additional);
                }

                let out_start = output.len();
                let remaining = max_size - out_start;
                let spare = output.spare_capacity_mut();
                let writable = spare.len().min(remaining);
                let spare = &mut spare[..writable.min(u32::MAX as usize)];
                offered = spare.len();
                (status, consumed, produced) =
                    self.decompress.inflate(&input[total_in..], spare)?;

                // SAFETY: inflate initialized exactly `produced` bytes in the spare capacity.
                // We're only extending the length by the number of bytes that were initialized.
                unsafe {
                    output.set_len(out_start + produced);
                }
            }

            total_in += consumed;

            if at_limit && produced != 0 {
                return Err(Error::MessageTooLarge);
            }

            if status == Status::StreamEnd {
                if total_in > data.len() {
                    return Err(Error::Compression(
                        "final DEFLATE block consumed its synthetic trailer".into(),
                    ));
                }
                // RFC 7692 permits BFINAL blocks. Start the next raw stream
                // while retaining the current message's LZ77 dictionary.
                self.decompress.reset(true)?;
                if total_in == data.len() {
                    // Do not feed the synthetic trailer to a fresh stream; it
                    // would leave a partial empty block before the next message.
                    break;
                }
                continue;
            }

            if consumed == 0 && produced == 0 {
                if total_in < input.len() {
                    return Err(Error::Compression("incomplete deflate payload".into()));
                }
                break;
            }

            // A full output slice can hide pending output after all input was
            // consumed. Continue once more with fresh capacity (or the limit
            // probe) until inflate stops filling the offered slice.
            if total_in >= input.len() && produced < offered {
                break;
            }
        }

        Ok(output.freeze())
    }

    /// Reset the decompression context (for no_context_takeover)
    pub fn reset(&mut self) {
        self.decompress
            .reset(false)
            .expect("failed to reset DEFLATE decoder");
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
