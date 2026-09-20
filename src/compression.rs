//! Compression management for WebSocket connections
//!
//! This module provides compression support with multiple modes:
//! - **Disabled**: No compression
//! - **Dedicated**: Each connection has its own compressor
//! - **Shared**: Connections share a pool of compressors
//! - **Window sizes**: Various window sizes for memory/compression tradeoffs

use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;

use crate::Compression;
use crate::deflate::{DeflateConfig, DeflateContext, DeflateDecoder, DeflateEncoder};
use crate::error::Result;

/// Number of compressors in the shared pool
const SHARED_POOL_SIZE: usize = 4;

/// A compression context that can be either dedicated or shared
pub enum CompressionContext {
    /// No compression
    Disabled,
    /// Dedicated per-connection compressor
    Dedicated(DeflateContext),
    /// Shared compressor from pool (encoder only, decoder is per-connection)
    Shared {
        pool: Arc<SharedCompressorPool>,
        decoder: DeflateDecoder,
        config: DeflateConfig,
    },
}

impl CompressionContext {
    /// Create a new compression context for the given mode (server role)
    pub fn server(mode: Compression) -> Self {
        match mode {
            Compression::Disabled => CompressionContext::Disabled,
            Compression::Shared => {
                let config = mode.to_deflate_config().unwrap();
                CompressionContext::Shared {
                    pool: Arc::new(SharedCompressorPool::new(config.clone())),
                    decoder: DeflateDecoder::new(
                        config.client_max_window_bits,
                        config.client_no_context_takeover,
                    ),
                    config,
                }
            }
            _ => {
                let config = mode.to_deflate_config().unwrap();
                CompressionContext::Dedicated(DeflateContext::server(config))
            }
        }
    }

    /// Create a new compression context for the given mode (client role)
    pub fn client(mode: Compression) -> Self {
        match mode {
            Compression::Disabled => CompressionContext::Disabled,
            Compression::Shared => {
                let config = mode.to_deflate_config().unwrap();
                CompressionContext::Shared {
                    pool: Arc::new(SharedCompressorPool::new(config.clone())),
                    decoder: DeflateDecoder::new(
                        config.server_max_window_bits,
                        config.server_no_context_takeover,
                    ),
                    config,
                }
            }
            _ => {
                let config = mode.to_deflate_config().unwrap();
                CompressionContext::Dedicated(DeflateContext::client(config))
            }
        }
    }

    /// Create a shared context that uses an existing pool
    pub fn with_shared_pool(pool: Arc<SharedCompressorPool>, is_server: bool) -> Self {
        let config = pool.config.clone();
        let decoder = if is_server {
            DeflateDecoder::new(
                config.client_max_window_bits,
                config.client_no_context_takeover,
            )
        } else {
            DeflateDecoder::new(
                config.server_max_window_bits,
                config.server_no_context_takeover,
            )
        };

        CompressionContext::Shared {
            pool,
            decoder,
            config,
        }
    }

    /// Check if compression is enabled
    #[inline]
    pub fn is_enabled(&self) -> bool {
        !matches!(self, CompressionContext::Disabled)
    }

    /// Compress a message payload
    ///
    /// Returns `None` if compression is disabled or if compression wouldn't reduce size.
    pub fn compress(&mut self, data: &[u8]) -> Result<Option<Bytes>> {
        match self {
            CompressionContext::Disabled => Ok(None),
            CompressionContext::Dedicated(ctx) => ctx.compress(data),
            CompressionContext::Shared { pool, config, .. } => {
                // Check threshold before acquiring encoder
                if data.len() < config.compression_threshold {
                    return Ok(None);
                }
                pool.compress(data)
            }
        }
    }

    /// Decompress a message payload
    pub fn decompress(&mut self, data: &[u8], max_size: usize) -> Result<Bytes> {
        match self {
            CompressionContext::Disabled => {
                // This shouldn't happen - protocol layer should not call decompress
                // if compression is disabled
                Ok(Bytes::copy_from_slice(data))
            }
            CompressionContext::Dedicated(ctx) => ctx.decompress(data, max_size),
            CompressionContext::Shared { decoder, .. } => decoder.decompress(data, max_size),
        }
    }

    /// Get the DeflateConfig for this context
    pub fn config(&self) -> Option<&DeflateConfig> {
        match self {
            CompressionContext::Disabled => None,
            CompressionContext::Dedicated(ctx) => Some(&ctx.config),
            CompressionContext::Shared { config, .. } => Some(config),
        }
    }
}

/// A pool of shared compressors for the `Shared` compression mode
///
/// This pool allows multiple connections to share compressor instances,
/// reducing memory usage when you have many connections.
pub struct SharedCompressorPool {
    /// Pool of encoders
    encoders: Vec<Mutex<DeflateEncoder>>,
    /// Configuration used for the pool
    config: DeflateConfig,
    /// Current encoder index (simple round-robin)
    next_encoder: std::sync::atomic::AtomicUsize,
}

impl SharedCompressorPool {
    /// Create a new shared compressor pool
    pub fn new(config: DeflateConfig) -> Self {
        let encoders = (0..SHARED_POOL_SIZE)
            .map(|_| {
                Mutex::new(DeflateEncoder::new(
                    config.server_max_window_bits,
                    true, // Always reset for shared mode (no context takeover)
                    config.compression_level,
                    config.compression_threshold,
                ))
            })
            .collect();

        Self {
            encoders,
            config,
            next_encoder: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Compress data using a pooled encoder
    pub fn compress(&self, data: &[u8]) -> Result<Option<Bytes>> {
        // Round-robin selection
        let idx = self
            .next_encoder
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % SHARED_POOL_SIZE;

        let mut encoder = self.encoders[idx].lock();
        encoder.compress(data)
    }

    /// Get the pool's configuration
    pub fn config(&self) -> &DeflateConfig {
        &self.config
    }
}

/// Global shared compressor pool for the default `Shared` mode
///
/// This is initialized lazily and provides a singleton pool for
/// all connections using `Compression::Shared`.
static GLOBAL_POOL: std::sync::OnceLock<Arc<SharedCompressorPool>> = std::sync::OnceLock::new();

/// Get the global shared compressor pool
pub fn global_shared_pool() -> Arc<SharedCompressorPool> {
    GLOBAL_POOL
        .get_or_init(|| {
            let config = Compression::Shared.to_deflate_config().unwrap();
            Arc::new(SharedCompressorPool::new(config))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeflateWindowBits;

    #[test]
    fn test_compression_context_disabled() {
        let mut ctx = CompressionContext::server(Compression::Disabled);
        assert!(!ctx.is_enabled());

        let result = ctx.compress(b"Hello, World!").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_compression_context_dedicated() {
        let mut ctx = CompressionContext::server(Compression::Dedicated);
        assert!(ctx.is_enabled());

        // Large enough to compress
        let data = b"Hello, World! This is a test message that should be compressed. ".repeat(10);
        let compressed = ctx.compress(&data).unwrap();
        assert!(compressed.is_some());

        let compressed = compressed.unwrap();
        assert!(compressed.len() < data.len());

        // Decompress
        let decompressed = ctx.decompress(&compressed, 1024 * 1024).unwrap();
        assert_eq!(decompressed.as_ref(), data.as_slice());
    }

    #[test]
    fn test_shared_pool() {
        let config = Compression::Shared.to_deflate_config().unwrap();
        let pool = SharedCompressorPool::new(config);

        // Large enough to compress
        let data = b"Hello, World! This is a test message that should be compressed. ".repeat(10);

        let compressed1 = pool.compress(&data).unwrap();
        let compressed2 = pool.compress(&data).unwrap();

        assert!(compressed1.is_some());
        assert!(compressed2.is_some());

        // Both should compress to similar sizes
        let c1 = compressed1.unwrap();
        let c2 = compressed2.unwrap();
        assert!(c1.len() < data.len());
        assert!(c2.len() < data.len());
    }

    #[test]
    fn test_compression_modes_configs() {
        // Test all modes produce valid configs
        for mode in [
            Compression::Disabled,
            Compression::Dedicated,
            Compression::Shared,
            Compression::Window1KB,
            Compression::Window2KB,
            Compression::Window4KB,
            Compression::Window8KB,
            Compression::Window16KB,
            Compression::Window32KB,
        ] {
            if mode == Compression::Disabled {
                assert!(mode.to_deflate_config().is_none());
            } else {
                let config = mode.to_deflate_config();
                assert!(config.is_some(), "Mode {:?} should have config", mode);
            }
        }
    }

    #[test]
    fn test_window_sizes() {
        assert_eq!(Compression::Disabled.window_bits(), None);
        assert_eq!(
            Compression::Window1KB.window_bits(),
            Some(DeflateWindowBits::Bits10)
        );
        assert_eq!(
            Compression::Window2KB.window_bits(),
            Some(DeflateWindowBits::Bits11)
        );
        assert_eq!(
            Compression::Window4KB.window_bits(),
            Some(DeflateWindowBits::Bits12)
        );
        assert_eq!(
            Compression::Window8KB.window_bits(),
            Some(DeflateWindowBits::Bits13)
        );
        assert_eq!(
            Compression::Window16KB.window_bits(),
            Some(DeflateWindowBits::Bits14)
        );
        assert_eq!(
            Compression::Window32KB.window_bits(),
            Some(DeflateWindowBits::Bits15)
        );
        assert_eq!(
            Compression::Dedicated.window_bits(),
            Some(DeflateWindowBits::Bits15)
        );
        assert_eq!(
            Compression::Shared.window_bits(),
            Some(DeflateWindowBits::Bits15)
        );
    }
}
