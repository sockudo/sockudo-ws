//! WebSocket frame masking utilities
//!
//! Re-exports from simd module with additional utilities.
//! Supports multiple RNG backends via feature flags:
//! - `fastrand`: fast PRNG (default, same as tokio-websockets)
//! - `getrandom`: cryptographically secure RNG
//! - `rand_rng`: rand's thread-local CSPRNG, seeded and periodically reseeded from the OS
//!
//! Native fastrand seeds from a clock and thread ID, not OS entropy. Use
//! `getrandom` or `rand_rng` when unpredictable output is required. Rand's
//! thread RNG is not automatically reseeded after a process fork; callers
//! using fork must follow rand's reseeding requirements.
//!
//! Builds without an RNG feature use a small standard-library fallback so
//! server-only `default-features = false` builds still compile.

pub use crate::simd::{apply_mask, apply_mask_offset};

#[cfg(not(any(feature = "fastrand", feature = "getrandom", feature = "rand_rng")))]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(any(feature = "fastrand", feature = "getrandom", feature = "rand_rng")))]
static MASK_FALLBACK_STATE: AtomicU64 = AtomicU64::new(0);
#[cfg(not(any(feature = "fastrand", feature = "getrandom", feature = "rand_rng")))]
static NONCE_FALLBACK_STATE: AtomicU64 = AtomicU64::new(0);

/// Generate a random mask for WebSocket client frames.
///
/// The RNG implementation is selected via feature flags:
/// - `fastrand` (default): fast, non-cryptographic PRNG
/// - `getrandom`: cryptographically secure RNG
/// - `rand_rng`: uses rand's OS-seeded thread-local CSPRNG
///
/// If multiple features are enabled, priority is: getrandom > rand_rng > fastrand.
/// If no RNG feature is enabled, a lightweight fallback is used. Enable
/// `fastrand`, `getrandom`, or `rand_rng` for production client masking.
#[inline]
pub fn generate_mask() -> [u8; 4] {
    generate_mask_inner()
}

/// Generate the 16 random bytes used by a WebSocket handshake key.
///
/// Uses the same backend priority as [`generate_mask`]: getrandom, rand_rng,
/// then fastrand. Selecting a cryptographic backend must also apply to the
/// public handshake nonce; the default fastrand backend is non-cryptographic.
#[inline]
pub(crate) fn generate_key_bytes() -> [u8; 16] {
    generate_key_bytes_inner()
}

#[cfg(all(
    feature = "fastrand",
    not(feature = "getrandom"),
    not(feature = "rand_rng")
))]
#[inline]
fn generate_key_bytes_inner() -> [u8; 16] {
    thread_local! {
        // Keep public nonce bytes from directly exposing consecutive outputs
        // of the frame-mask RNG. Fork the thread RNG once at initialization;
        // subsequent nonces do not consume it. This is not cryptographic isolation.
        static NONCE_RNG: std::cell::RefCell<fastrand::Rng> =
            std::cell::RefCell::new(fastrand::Rng::new());
    }
    let mut bytes = [0u8; 16];
    NONCE_RNG.with(|rng| rng.borrow_mut().fill(&mut bytes));
    bytes
}

#[cfg(all(feature = "rand_rng", not(feature = "getrandom")))]
#[inline]
fn generate_key_bytes_inner() -> [u8; 16] {
    use rand::Rng;
    let mut bytes = [0; 16];
    rand::rng().fill(&mut bytes[..]);
    bytes
}

#[cfg(feature = "getrandom")]
#[inline]
fn generate_key_bytes_inner() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("getrandom failed");
    bytes
}

#[cfg(not(any(feature = "fastrand", feature = "getrandom", feature = "rand_rng")))]
#[inline]
fn generate_key_bytes_inner() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&generate_fallback(&NONCE_FALLBACK_STATE));
    }
    bytes
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
    generate_fallback(&MASK_FALLBACK_STATE)
}

#[cfg(not(any(feature = "fastrand", feature = "getrandom", feature = "rand_rng")))]
#[inline]
fn generate_fallback(state: &AtomicU64) -> [u8; 4] {
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut x = state.load(Ordering::Relaxed);
    if x == 0 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let state_addr = (state as *const AtomicU64 as usize) as u64;
        x = nanos ^ state_addr.rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15;
    }

    // xorshift64*: adequate for no-RNG test/server-only builds, not a CSPRNG.
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    state.store(x, Ordering::Relaxed);

    let value = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
    (value as u32).to_ne_bytes()
}

#[cfg(all(
    test,
    not(any(feature = "fastrand", feature = "getrandom", feature = "rand_rng"))
))]
mod tests {
    use super::*;

    #[test]
    fn handshake_nonce_does_not_advance_the_fallback_mask_stream() {
        // Other library tests generate masks concurrently under cargo test.
        // Re-run only this test in a child process before resetting global state.
        const CHILD: &str = "SOCKUDO_FALLBACK_RNG_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let name = concat!(
                module_path!(),
                "::handshake_nonce_does_not_advance_the_fallback_mask_stream"
            );
            let name = name.split_once("::").unwrap().1;
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--test-threads=1", "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "isolated test failed: {output:?}");
            return;
        }
        MASK_FALLBACK_STATE.store(42, Ordering::Relaxed);
        let expected = generate_mask();

        MASK_FALLBACK_STATE.store(42, Ordering::Relaxed);
        NONCE_FALLBACK_STATE.store(42, Ordering::Relaxed);
        let _ = generate_key_bytes();
        let actual = generate_mask();

        assert_eq!(actual, expected);
    }
}
