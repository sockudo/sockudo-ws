//! WebSocket frame masking utilities
//!
//! Re-exports from simd module with additional utilities.
//! Supports multiple RNG backends via feature flags:
//! - `fastrand`: fast PRNG (default, same as tokio-websockets)
//! - `getrandom`: cryptographically secure RNG
//! - `rand_rng`: alternative PRNG (prefer if `rand` is already in dependency tree)
//!
//! Builds without an RNG feature use a small standard-library fallback so
//! server-only `default-features = false` builds still compile.

pub use crate::simd::{apply_mask, apply_mask_offset};

/// Generate a random mask for WebSocket client frames.
///
/// The RNG implementation is selected via feature flags:
/// - `fastrand` (default): fast, non-cryptographic PRNG
/// - `getrandom`: cryptographically secure RNG
/// - `rand_rng`: uses the `rand` crate
///
/// If multiple features are enabled, priority is: getrandom > rand_rng > fastrand.
/// If no RNG feature is enabled, a lightweight fallback is used. Enable
/// `fastrand`, `getrandom`, or `rand_rng` for production client masking.
#[inline]
pub fn generate_mask() -> [u8; 4] {
    generate_mask_inner()
}

#[cfg(feature = "getrandom")]
#[inline]
fn generate_mask_inner() -> [u8; 4] {
    let mut buf = [0u8; 4];
    getrandom::getrandom(&mut buf).expect("getrandom failed");
    buf
}

#[cfg(all(feature = "rand_rng", not(feature = "getrandom")))]
#[inline]
fn generate_mask_inner() -> [u8; 4] {
    use rand::Rng;
    rand::rng().random()
}

#[cfg(all(
    feature = "fastrand",
    not(feature = "getrandom"),
    not(feature = "rand_rng")
))]
#[inline]
fn generate_mask_inner() -> [u8; 4] {
    fastrand::u32(..).to_ne_bytes()
}

#[cfg(not(any(feature = "fastrand", feature = "getrandom", feature = "rand_rng")))]
#[inline]
fn generate_mask_inner() -> [u8; 4] {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static STATE: AtomicU64 = AtomicU64::new(0);

    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let stack_addr = (&x as *const u64 as usize) as u64;
        x = nanos ^ stack_addr.rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15;
    }

    // xorshift64*: adequate for no-RNG test/server-only builds, not a CSPRNG.
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    STATE.store(x, Ordering::Relaxed);

    let value = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
    (value as u32).to_ne_bytes()
}
