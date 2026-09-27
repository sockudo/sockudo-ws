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
        match mode.to_deflate_config() {
            None => Self::Disabled,
            Some(_) if mode.is_shared() => Self::with_shared_pool(global_shared_pool(), true),
            Some(config) => Self::Dedicated(DeflateContext::server(config)),
        }
    }

    /// Create a new compression context for the given mode (client role)
    pub fn client(mode: Compression) -> Self {
        match mode.to_deflate_config() {
            None => Self::Disabled,
            Some(_) if mode.is_shared() => Self::with_shared_pool(global_shared_pool(), false),
            Some(config) => Self::Dedicated(DeflateContext::client(config)),
        }
    }

    /// Create a shared context that uses an existing pool.
    ///
    /// The context stores a role-specific handle to the pool's encoder sets,
    /// rather than retaining the supplied [`Arc`] itself.
    pub fn with_shared_pool(pool: Arc<SharedCompressorPool>, is_server: bool) -> Self {
        let config = pool.config().clone();
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

        let pool = Arc::new(pool.for_role(is_server));

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
/// This pool allows multiple connections to share four synchronous compressor
/// instances, reducing encoder memory at the cost of possible contention.
struct SharedEncoderPool {
    /// Pool of encoders
    encoders: Vec<Mutex<DeflateEncoder>>,
    /// Current encoder index (simple round-robin)
    next_encoder: std::sync::atomic::AtomicUsize,
}

impl SharedEncoderPool {
    fn new(config: &DeflateConfig, window_bits: crate::deflate::DeflateWindowBits) -> Self {
        let encoders = (0..SHARED_POOL_SIZE)
            .map(|_| {
                Mutex::new(DeflateEncoder::new(
                    window_bits,
                    // Reset between messages because pool users can be different connections.
                    true,
                    config.compression_level,
                    config.compression_threshold,
                ))
            })
            .collect();

        Self {
            encoders,
            next_encoder: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn compress(&self, data: &[u8]) -> Result<Option<Bytes>> {
        // Round-robin selection
        let index = self
            .next_encoder
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % SHARED_POOL_SIZE;
        self.encoders[index].lock().compress(data)
    }
}

struct SharedCompressorPoolInner {
    server: Arc<SharedEncoderPool>,
    client: Arc<SharedEncoderPool>,
    /// Configuration used for both role-specific pools
    config: DeflateConfig,
}

/// Shared server and client encoder pools selected by the sending role.
///
/// Contexts share four synchronous encoder instances per distinct role window,
/// reducing encoder memory at the cost of possible contention. Compression runs
/// on the caller thread and waits synchronously when the selected slot is busy.
pub struct SharedCompressorPool {
    inner: Arc<SharedCompressorPoolInner>,
    is_server: bool,
}

impl SharedCompressorPool {
    /// Create a new shared compressor pool for server-side compression
    ///
    /// [`CompressionContext::with_shared_pool`] selects the matching sending
    /// direction when the pool is attached to a client context.
    pub fn new(config: DeflateConfig) -> Self {
        let server = Arc::new(SharedEncoderPool::new(
            &config,
            config.server_max_window_bits,
        ));
        let client = if config.server_max_window_bits == config.client_max_window_bits {
            Arc::clone(&server)
        } else {
            Arc::new(SharedEncoderPool::new(
                &config,
                config.client_max_window_bits,
            ))
        };

        Self {
            inner: Arc::new(SharedCompressorPoolInner {
                server,
                client,
                config,
            }),
            is_server: true,
        }
    }

    fn for_role(&self, is_server: bool) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            is_server,
        }
    }

    /// Compress data using a pooled encoder
    pub fn compress(&self, data: &[u8]) -> Result<Option<Bytes>> {
        let encoders = if self.is_server {
            &self.inner.server
        } else {
            &self.inner.client
        };
        encoders.compress(data)
    }

    /// Get the pool's configuration
    pub fn config(&self) -> &DeflateConfig {
        &self.inner.config
    }
}

/// Global shared compressor pool for the default `Shared` mode
///
/// This is initialized lazily and provides a singleton pool for
/// all connections using `Compression::Shared`. Compression runs on the caller
/// thread and waits synchronously when the selected encoder slot is busy.
static GLOBAL_POOL: std::sync::OnceLock<Arc<SharedCompressorPool>> = std::sync::OnceLock::new();

/// Get the global shared compressor pool
pub fn global_shared_pool() -> Arc<SharedCompressorPool> {
    GLOBAL_POOL
        .get_or_init(|| {
            let config = Compression::Shared
                .to_deflate_config()
                .expect("shared compression has a deflate configuration");
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
    fn shared_contexts_reuse_the_global_encoder_pool() {
        let server_one = CompressionContext::server(Compression::Shared);
        let server_two = CompressionContext::server(Compression::Shared);
        let client = CompressionContext::client(Compression::Shared);

        let CompressionContext::Shared {
            pool: server_one, ..
        } = server_one
        else {
            panic!("server context must be shared");
        };
        let CompressionContext::Shared {
            pool: server_two, ..
        } = server_two
        else {
            panic!("server context must be shared");
        };
        let CompressionContext::Shared { pool: client, .. } = client else {
            panic!("client context must be shared");
        };

        assert!(Arc::ptr_eq(&server_one.inner, &server_two.inner));
        assert!(Arc::ptr_eq(&server_one.inner, &client.inner));
        assert!(server_one.is_server);
        assert!(!client.is_server);
        assert!(Arc::ptr_eq(
            &server_one.inner.server,
            &server_one.inner.client
        ));
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
