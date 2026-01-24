//! WebSocket frame masking utilities
//!
//! Re-exports from simd module with additional utilities.
//! Supports multiple RNG backends via feature flags:
//! - `fastrand`: fast PRNG (default, same as tokio-websockets)
//! - `getrandom`: cryptographically secure RNG
//! - `rand_rng`: alternative PRNG (prefer if `rand` is already in dependency tree)

pub use crate::simd::{apply_mask, apply_mask_offset};

/// Generate a random mask for WebSocket client frames.
///
/// The RNG implementation is selected via feature flags:
/// - `fastrand` (default): fast, non-cryptographic PRNG
/// - `getrandom`: cryptographically secure RNG
/// - `rand_rng`: uses the `rand` crate
///
/// If multiple features are enabled, priority is: getrandom > rand_rng > fastrand.
/// If no RNG feature is enabled, this function will fail to compile.
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
fn generate_mask_inner() -> [u8; 4] {
    compile_error!("At least one RNG feature must be enabled: fastrand, getrandom, or rand_rng");
}
