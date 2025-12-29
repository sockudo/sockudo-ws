//! WebSocket frame masking utilities
//!
//! Re-exports from simd module with additional utilities

pub use crate::simd::{apply_mask, apply_mask_offset, generate_mask};

/// Fast mask generation using thread-local state
#[cfg(feature = "tokio-runtime")]
mod fast_rng {
    use std::cell::Cell;

    thread_local! {
        static RNG_STATE: Cell<u64> = Cell::new(0);
    }

    /// Generate a random mask using xorshift64
    ///
    /// This is faster than generate_mask() for repeated calls
    /// as it maintains thread-local state.
    #[inline]
    pub fn generate_mask_fast() -> [u8; 4] {
        RNG_STATE.with(|state| {
            let mut s = state.get();

            // Initialize on first use
            if s == 0 {
                s = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                // Mix in pointer address for uniqueness (stable alternative to thread_id_value)
                s ^= &s as *const _ as u64;
            }

            // xorshift64
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;

            state.set(s);
            (s as u32).to_ne_bytes()
        })
    }
}

#[cfg(feature = "tokio-runtime")]
pub use fast_rng::generate_mask_fast;

#[cfg(not(feature = "tokio-runtime"))]
pub use crate::simd::generate_mask as generate_mask_fast;
