//! SIMD-accelerated operations for WebSocket processing
//!
//! This module provides highly optimized implementations of:
//! - Frame masking/unmasking (XOR operations)
//! - UTF-8 validation
//! - HTTP header parsing helpers
//!
//! The implementations automatically select the best available SIMD instruction set:
//! - AVX-512 (512-bit, 64 bytes per iteration) - x86_64
//! - AVX2 (256-bit, 32 bytes per iteration) - x86_64
//! - SSE2 (128-bit, 16 bytes per iteration) - x86_64
//! - NEON (128-bit) - aarch64, arm (nightly)
//! - LASX (256-bit, 32 bytes per iteration) - loongarch64 (nightly)
//! - LSX (128-bit, 16 bytes per iteration) - loongarch64 (nightly)
//! - AltiVec (128-bit, 16 bytes per iteration) - powerpc/powerpc64 (nightly)
//! - z13 vectors (128-bit, 16 bytes per iteration) - s390x (nightly)
//! - Scalar fallback
//!
//! # Alignment Strategy
//!
//! For optimal performance, all SIMD implementations use an alignment-aware strategy:
//! 1. Process unaligned prefix bytes with scalar operations
//! 2. Process aligned chunks with SIMD operations
//! 3. Process unaligned suffix bytes with scalar operations
//!
//! This ensures optimal memory access patterns and avoids potential performance
//! penalties from unaligned loads/stores on some architectures.
//!
//! # Architecture Support
//!
//! | Architecture  | Instructions | Stable | Nightly |
//! |---------------|--------------|--------|---------|
//! | x86_64        | SSE2         | ✅      | ✅       |
//! | x86_64        | AVX2         | ✅      | ✅       |
//! | x86_64        | AVX-512      | ✅      | ✅       |
//! | aarch64       | NEON         | ✅      | ✅       |
//! | arm           | NEON         | ❌      | ✅       |
//! | loongarch64   | LSX          | ❌      | ✅       |
//! | loongarch64   | LASX         | ❌      | ✅       |
//! | powerpc       | AltiVec      | ❌      | ✅       |
//! | powerpc64     | AltiVec      | ❌      | ✅       |
//! | s390x         | z13 vectors  | ❌      | ✅       |

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

/// Apply WebSocket mask using the fastest available SIMD instructions
///
/// This function XORs the data in-place with a repeating 4-byte mask.
/// It's used for both masking (client->server) and unmasking (server reads).
///
/// # Safety
/// The data slice must be valid for reading and writing.
#[inline]
pub fn apply_mask(data: &mut [u8], mask: [u8; 4]) {
    if data.is_empty() {
        return;
    }

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f") && is_x86_feature_detected!("avx512bw") {
            unsafe { apply_mask_avx512_aligned(data, mask) };
            return;
        }
        if is_x86_feature_detected!("avx2") {
            unsafe { apply_mask_avx2_aligned(data, mask) };
            return;
        }
        if is_x86_feature_detected!("sse2") {
            unsafe { apply_mask_sse2_aligned(data, mask) };
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        unsafe { apply_mask_neon_aligned(data, mask) };
    }

    #[cfg(target_arch = "loongarch64")]
    {
        unsafe { apply_mask_loongarch(data, mask) };
        return;
    }

    #[cfg(any(target_arch = "powerpc", target_arch = "powerpc64"))]
    {
        unsafe { apply_mask_altivec(data, mask) };
        return;
    }

    #[cfg(target_arch = "s390x")]
    {
        unsafe { apply_mask_s390x(data, mask) };
        return;
    }

    #[cfg(target_arch = "arm")]
    {
        unsafe { apply_mask_arm_neon(data, mask) };
        return;
    }

    // Scalar fallback for unsupported architectures
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "loongarch64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "s390x",
        target_arch = "arm"
    )))]
    apply_mask_scalar(data, mask);
}

/// Scalar fallback for mask application
#[inline]
fn apply_mask_scalar(data: &mut [u8], mask: [u8; 4]) {
    apply_mask_scalar_offset(data, mask, 0);
}

/// Scalar fallback for mask application with offset
///
/// The offset parameter allows starting at a different position in the mask,
/// which is needed for continuation frames and alignment handling.
#[inline]
fn apply_mask_scalar_offset(data: &mut [u8], mask: [u8; 4], offset: usize) {
    let len = data.len();
    if len == 0 {
        return;
    }

    let mut i = 0;
    let mut mask_idx = offset & 3;

    // Handle unaligned prefix to get to 8-byte alignment
    let ptr_addr = data.as_ptr() as usize;
    while i < len && (ptr_addr + i) & 7 != 0 {
        data[i] ^= mask[mask_idx];
        mask_idx = (mask_idx + 1) & 3;
        i += 1;
    }

    // Process 8 bytes at a time using u64
    if i + 8 <= len {
        // Create mask_u64 starting from current mask position
        let mask_u64 = u64::from_ne_bytes([
            mask[mask_idx],
            mask[(mask_idx + 1) & 3],
            mask[(mask_idx + 2) & 3],
            mask[(mask_idx + 3) & 3],
            mask[mask_idx],
            mask[(mask_idx + 1) & 3],
            mask[(mask_idx + 2) & 3],
            mask[(mask_idx + 3) & 3],
        ]);

        while i + 8 <= len {
            // SAFETY: We checked that i + 8 <= len and the pointer is aligned to 8 bytes
            unsafe {
                let ptr = data.as_mut_ptr().add(i) as *mut u64;
                *ptr ^= mask_u64;
            }
            i += 8;
        }
    }

    // Handle remaining bytes
    while i < len {
        data[i] ^= mask[mask_idx];
        mask_idx = (mask_idx + 1) & 3;
        i += 1;
    }
}

/// AVX-512 implementation with alignment handling (64 bytes per iteration)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "avx512bw")]
unsafe fn apply_mask_avx512_aligned(data: &mut [u8], mask: [u8; 4]) {
    unsafe {
        let len = data.len();
        if len == 0 {
            return;
        }

        let mut ptr = data.as_mut_ptr();
        let mut remaining = len;
        let mut mask_offset = 0usize;

        // Handle unaligned prefix (align to 64-byte boundary for optimal AVX-512 performance)
        let align_offset = (ptr as usize) & 63;
        if align_offset != 0 {
            let prefix_len = (64 - align_offset).min(remaining);
            let prefix = std::slice::from_raw_parts_mut(ptr, prefix_len);
            apply_mask_scalar_offset(prefix, mask, mask_offset);
            ptr = ptr.add(prefix_len);
            remaining -= prefix_len;
            mask_offset = (mask_offset + prefix_len) & 3;
        }

        // Create 64-byte mask vector with correct rotation
        let rotated_mask = [
            mask[mask_offset],
            mask[(mask_offset + 1) & 3],
            mask[(mask_offset + 2) & 3],
            mask[(mask_offset + 3) & 3],
        ];
        let mask_u32 = u32::from_ne_bytes(rotated_mask);
        let mask_vec = _mm512_set1_epi32(mask_u32 as i32);

        // Process 64 bytes at a time (now aligned)
        while remaining >= 64 {
            let data_vec = _mm512_load_si512(ptr as *const __m512i);
            let result = _mm512_xor_si512(data_vec, mask_vec);
            _mm512_store_si512(ptr as *mut __m512i, result);
            ptr = ptr.add(64);
            remaining -= 64;
            // mask_offset stays the same since 64 % 4 == 0
        }

        // Handle remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts_mut(ptr, remaining);
            apply_mask_scalar_offset(slice, mask, mask_offset);
        }
    }
}

/// AVX2 implementation with alignment handling (32 bytes per iteration)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn apply_mask_avx2_aligned(data: &mut [u8], mask: [u8; 4]) {
    unsafe {
        let len = data.len();
        if len == 0 {
            return;
        }

        let mut ptr = data.as_mut_ptr();
        let mut remaining = len;
        let mut mask_offset = 0usize;

        // Handle unaligned prefix (align to 32-byte boundary for optimal AVX2 performance)
        let align_offset = (ptr as usize) & 31;
        if align_offset != 0 {
            let prefix_len = (32 - align_offset).min(remaining);
            let prefix = std::slice::from_raw_parts_mut(ptr, prefix_len);
            apply_mask_scalar_offset(prefix, mask, mask_offset);
            ptr = ptr.add(prefix_len);
            remaining -= prefix_len;
            mask_offset = (mask_offset + prefix_len) & 3;
        }

        // Create 32-byte mask vector with correct rotation
        let rotated_mask = [
            mask[mask_offset],
            mask[(mask_offset + 1) & 3],
            mask[(mask_offset + 2) & 3],
            mask[(mask_offset + 3) & 3],
        ];
        let mask_u32 = u32::from_ne_bytes(rotated_mask);
        let mask_vec = _mm256_set1_epi32(mask_u32 as i32);

        // Process 32 bytes at a time (now aligned)
        while remaining >= 32 {
            let data_vec = _mm256_load_si256(ptr as *const __m256i);
            let result = _mm256_xor_si256(data_vec, mask_vec);
            _mm256_store_si256(ptr as *mut __m256i, result);
            ptr = ptr.add(32);
            remaining -= 32;
            // mask_offset stays the same since 32 % 4 == 0
        }

        // Process remaining 16 bytes with SSE2 if available
        if remaining >= 16 {
            let mask_vec_128 = _mm_set1_epi32(mask_u32 as i32);
            let data_vec = _mm_loadu_si128(ptr as *const __m128i);
            let result = _mm_xor_si128(data_vec, mask_vec_128);
            _mm_storeu_si128(ptr as *mut __m128i, result);
            ptr = ptr.add(16);
            remaining -= 16;
        }

        // Handle remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts_mut(ptr, remaining);
            apply_mask_scalar_offset(slice, mask, mask_offset);
        }
    }
}

/// SSE2 implementation with alignment handling (16 bytes per iteration)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn apply_mask_sse2_aligned(data: &mut [u8], mask: [u8; 4]) {
    unsafe {
        let len = data.len();
        if len == 0 {
            return;
        }

        let mut ptr = data.as_mut_ptr();
        let mut remaining = len;
        let mut mask_offset = 0usize;

        // Handle unaligned prefix (align to 16-byte boundary for optimal SSE2 performance)
        let align_offset = (ptr as usize) & 15;
        if align_offset != 0 {
            let prefix_len = (16 - align_offset).min(remaining);
            let prefix = std::slice::from_raw_parts_mut(ptr, prefix_len);
            apply_mask_scalar_offset(prefix, mask, mask_offset);
            ptr = ptr.add(prefix_len);
            remaining -= prefix_len;
            mask_offset = (mask_offset + prefix_len) & 3;
        }

        // Create 16-byte mask vector with correct rotation
        let rotated_mask = [
            mask[mask_offset],
            mask[(mask_offset + 1) & 3],
            mask[(mask_offset + 2) & 3],
            mask[(mask_offset + 3) & 3],
        ];
        let mask_u32 = u32::from_ne_bytes(rotated_mask);
        let mask_vec = _mm_set1_epi32(mask_u32 as i32);

        // Process 16 bytes at a time (now aligned)
        while remaining >= 16 {
            let data_vec = _mm_load_si128(ptr as *const __m128i);
            let result = _mm_xor_si128(data_vec, mask_vec);
            _mm_store_si128(ptr as *mut __m128i, result);
            ptr = ptr.add(16);
            remaining -= 16;
            // mask_offset stays the same since 16 % 4 == 0
        }

        // Handle remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts_mut(ptr, remaining);
            apply_mask_scalar_offset(slice, mask, mask_offset);
        }
    }
}

/// NEON implementation for ARM64 with alignment handling (16 bytes per iteration)
#[cfg(target_arch = "aarch64")]
unsafe fn apply_mask_neon_aligned(data: &mut [u8], mask: [u8; 4]) {
    unsafe {
        let len = data.len();
        if len == 0 {
            return;
        }

        let mut ptr = data.as_mut_ptr();
        let mut remaining = len;
        let mut mask_offset = 0usize;

        // Handle unaligned prefix (align to 16-byte boundary for optimal NEON performance)
        let align_offset = (ptr as usize) & 15;
        if align_offset != 0 {
            let prefix_len = (16 - align_offset).min(remaining);
            let prefix = std::slice::from_raw_parts_mut(ptr, prefix_len);
            apply_mask_scalar_offset(prefix, mask, mask_offset);
            ptr = ptr.add(prefix_len);
            remaining -= prefix_len;
            mask_offset = (mask_offset + prefix_len) & 3;
        }

        // Create 16-byte mask vector with correct rotation
        let rotated_mask = [
            mask[mask_offset],
            mask[(mask_offset + 1) & 3],
            mask[(mask_offset + 2) & 3],
            mask[(mask_offset + 3) & 3],
        ];
        let mask_bytes: [u8; 16] = [
            rotated_mask[0],
            rotated_mask[1],
            rotated_mask[2],
            rotated_mask[3],
            rotated_mask[0],
            rotated_mask[1],
            rotated_mask[2],
            rotated_mask[3],
            rotated_mask[0],
            rotated_mask[1],
            rotated_mask[2],
            rotated_mask[3],
            rotated_mask[0],
            rotated_mask[1],
            rotated_mask[2],
            rotated_mask[3],
        ];
        let mask_vec = vld1q_u8(mask_bytes.as_ptr());

        // Process 16 bytes at a time (now aligned)
        while remaining >= 16 {
            let data_vec = vld1q_u8(ptr);
            let result = veorq_u8(data_vec, mask_vec);
            vst1q_u8(ptr, result);
            ptr = ptr.add(16);
            remaining -= 16;
            // mask_offset stays the same since 16 % 4 == 0
        }

        // Handle remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts_mut(ptr, remaining);
            apply_mask_scalar_offset(slice, mask, mask_offset);
        }
    }
}

/// LoongArch64 implementation using LSX/LASX SIMD
///
/// On nightly with the `nightly` feature, uses native LASX (256-bit) or LSX (128-bit).
/// Otherwise falls back to optimized scalar implementation.
#[cfg(target_arch = "loongarch64")]
unsafe fn apply_mask_loongarch(data: &mut [u8], mask: [u8; 4]) {
    #[cfg(feature = "nightly")]
    {
        // Try LASX first (256-bit), then LSX (128-bit)
        if std::arch::is_loongarch_feature_detected!("lasx") {
            apply_mask_lasx(data, mask);
            return;
        }
        if std::arch::is_loongarch_feature_detected!("lsx") {
            apply_mask_lsx(data, mask);
            return;
        }
    }
    // Fallback to scalar
    apply_mask_scalar(data, mask);
}

/// LoongArch64 LASX implementation (256-bit, 32 bytes per iteration)
#[cfg(all(target_arch = "loongarch64", feature = "nightly"))]
#[target_feature(enable = "lasx")]
unsafe fn apply_mask_lasx(data: &mut [u8], mask: [u8; 4]) {
    use std::arch::loongarch64::{lasx_xvld, lasx_xvst, lasx_xvxor_v, v32i8};

    unsafe {
        let (prefix, aligned_data, suffix) = data.align_to_mut::<v32i8>();

        if !prefix.is_empty() {
            apply_mask_scalar(prefix, mask);
        }

        if !aligned_data.is_empty() {
            // Create 256-bit mask by repeating the 4-byte pattern 8 times
            let key_vector: [i32; 8] = [i32::from_ne_bytes(mask); 8];
            let mask_vec = lasx_xvld::<0>(key_vector.as_ptr().cast());

            for block in aligned_data {
                *block = lasx_xvxor_v(*block, mask_vec);
            }
        }

        if !suffix.is_empty() {
            apply_mask_scalar(suffix, mask);
        }
    }
}

/// LoongArch64 LSX implementation (128-bit, 16 bytes per iteration)
#[cfg(all(target_arch = "loongarch64", feature = "nightly"))]
#[target_feature(enable = "lsx")]
unsafe fn apply_mask_lsx(data: &mut [u8], mask: [u8; 4]) {
    use std::arch::loongarch64::{lsx_vld, lsx_vst, lsx_vxor_v, v16i8};

    unsafe {
        let (prefix, aligned_data, suffix) = data.align_to_mut::<v16i8>();

        if !prefix.is_empty() {
            apply_mask_scalar(prefix, mask);
        }

        if !aligned_data.is_empty() {
            // Create 128-bit mask by repeating the 4-byte pattern 4 times
            let key_vector: [i32; 4] = [i32::from_ne_bytes(mask); 4];
            let mask_vec = lsx_vld(key_vector.as_ptr().cast(), 0);

            for block in aligned_data {
                *block = lsx_vxor_v(*block, mask_vec);
            }
        }

        if !suffix.is_empty() {
            apply_mask_scalar(suffix, mask);
        }
    }
}

/// PowerPC AltiVec implementation (16 bytes per iteration)
///
/// Uses VMX/AltiVec SIMD instructions available on PowerPC/PowerPC64.
#[cfg(any(target_arch = "powerpc", target_arch = "powerpc64"))]
unsafe fn apply_mask_altivec(data: &mut [u8], mask: [u8; 4]) {
    #[cfg(feature = "nightly")]
    {
        apply_mask_altivec_impl(data, mask);
        return;
    }
    #[cfg(not(feature = "nightly"))]
    {
        apply_mask_scalar(data, mask);
    }
}

/// PowerPC AltiVec implementation (nightly only)
#[cfg(all(
    any(target_arch = "powerpc", target_arch = "powerpc64"),
    feature = "nightly"
))]
#[target_feature(enable = "altivec")]
unsafe fn apply_mask_altivec_impl(data: &mut [u8], mask: [u8; 4]) {
    #[cfg(target_arch = "powerpc")]
    use std::arch::powerpc::{vec_splats, vec_xor, vector_unsigned_char};
    #[cfg(target_arch = "powerpc64")]
    use std::arch::powerpc64::{vec_splats, vec_xor, vector_unsigned_char};

    unsafe {
        let (prefix, aligned_data, suffix) = data.align_to_mut::<vector_unsigned_char>();

        if !prefix.is_empty() {
            apply_mask_scalar(prefix, mask);
        }

        if !aligned_data.is_empty() {
            let mask_vec: vector_unsigned_char =
                std::mem::transmute(vec_splats(i32::from_ne_bytes(mask)));

            for block in aligned_data {
                *block = vec_xor(*block, mask_vec);
            }
        }

        if !suffix.is_empty() {
            apply_mask_scalar(suffix, mask);
        }
    }
}

/// s390x z13 vector implementation (16 bytes per iteration)
///
/// Uses the z13 Vector Facility available on IBM z Systems.
#[cfg(target_arch = "s390x")]
unsafe fn apply_mask_s390x(data: &mut [u8], mask: [u8; 4]) {
    #[cfg(feature = "nightly")]
    {
        apply_mask_s390x_impl(data, mask);
        return;
    }
    #[cfg(not(feature = "nightly"))]
    {
        apply_mask_scalar(data, mask);
    }
}

/// s390x z13 vector implementation (nightly only)
#[cfg(all(target_arch = "s390x", feature = "nightly"))]
#[target_feature(enable = "vector")]
unsafe fn apply_mask_s390x_impl(data: &mut [u8], mask: [u8; 4]) {
    use std::arch::s390x::{vec_splats, vec_xor, vector_signed_int, vector_unsigned_char};

    unsafe {
        let (prefix, aligned_data, suffix) = data.align_to_mut::<vector_unsigned_char>();

        if !prefix.is_empty() {
            apply_mask_scalar(prefix, mask);
        }

        if !aligned_data.is_empty() {
            let mask_vec: vector_unsigned_char =
                std::mem::transmute(vec_splats::<i32, vector_signed_int>(i32::from_ne_bytes(
                    mask,
                )));

            for block in aligned_data {
                *block = vec_xor(*block, mask_vec);
            }
        }

        if !suffix.is_empty() {
            apply_mask_scalar(suffix, mask);
        }
    }
}

/// ARM 32-bit NEON implementation (16 bytes per iteration)
///
/// Available on nightly Rust only for 32-bit ARM targets.
#[cfg(target_arch = "arm")]
unsafe fn apply_mask_arm_neon(data: &mut [u8], mask: [u8; 4]) {
    #[cfg(feature = "nightly")]
    {
        if std::arch::is_arm_feature_detected!("neon") {
            apply_mask_arm_neon_impl(data, mask);
            return;
        }
    }
    apply_mask_scalar(data, mask);
}

/// ARM 32-bit NEON implementation (nightly only)
#[cfg(all(target_arch = "arm", feature = "nightly"))]
#[target_feature(enable = "neon")]
unsafe fn apply_mask_arm_neon_impl(data: &mut [u8], mask: [u8; 4]) {
    use std::arch::arm::{uint8x16_t, veorq_u8, vld1q_dup_s32, vreinterpretq_u8_s32};

    unsafe {
        let (prefix, aligned_data, suffix) = data.align_to_mut::<uint8x16_t>();

        if !prefix.is_empty() {
            apply_mask_scalar(prefix, mask);
        }

        if !aligned_data.is_empty() {
            let key_i32 = mask.as_ptr().cast::<i32>().read_unaligned();
            let mask_vec = vreinterpretq_u8_s32(vld1q_dup_s32(&key_i32));

            for block in aligned_data {
                *block = veorq_u8(*block, mask_vec);
            }
        }

        if !suffix.is_empty() {
            apply_mask_scalar(suffix, mask);
        }
    }
}

/// Apply mask with offset (for continuation frames)
///
/// When unmasking across multiple reads, the offset determines which
/// byte of the mask to start with.
#[inline]
pub fn apply_mask_offset(data: &mut [u8], mask: [u8; 4], offset: usize) {
    if offset == 0 {
        apply_mask(data, mask);
        return;
    }

    // Rotate mask to account for offset
    let offset = offset & 3;
    let rotated_mask = [
        mask[offset & 3],
        mask[(offset + 1) & 3],
        mask[(offset + 2) & 3],
        mask[(offset + 3) & 3],
    ];
    apply_mask(data, rotated_mask);
}

/// Generate a random mask for client frames
///
/// Uses the configured random number generator:
/// - `fastrand` feature: fast PRNG (default, same as tokio-websockets)
/// - `getrandom` feature: cryptographically secure RNG
/// - `rand_rng` feature: uses rand crate
/// - `nightly` feature: uses std::random (cryptographically secure)
///
/// If no RNG feature is enabled, falls back to a simple counter-based approach
/// (not suitable for production use).
#[inline]
pub fn generate_mask() -> [u8; 4] {
    #[cfg(feature = "fastrand")]
    {
        fastrand::u32(..).to_ne_bytes()
    }

    #[cfg(all(feature = "getrandom", not(feature = "fastrand")))]
    {
        let mut buf = [0u8; 4];
        getrandom::getrandom(&mut buf).expect("getrandom failed");
        return buf;
    }

    #[cfg(all(
        feature = "rand_rng",
        not(feature = "fastrand"),
        not(feature = "getrandom")
    ))]
    {
        use rand::Rng;
        return rand::rng().random::<[u8; 4]>();
    }

    // Fallback: use a simple counter-based approach (NOT cryptographically secure)
    // This should only be used for testing or when no RNG feature is enabled
    #[cfg(not(any(feature = "fastrand", feature = "getrandom", feature = "rand_rng")))]
    {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0x12345678);
        let val = COUNTER.fetch_add(0x9E3779B9, Ordering::Relaxed); // Golden ratio constant
        val.to_ne_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_mask_basic() {
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let original = b"Hello, WebSocket!".to_vec();
        let mut data = original.clone();

        // Apply mask
        apply_mask(&mut data, mask);

        // Data should be different
        assert_ne!(data, original);

        // Apply mask again (XOR is self-inverse)
        apply_mask(&mut data, mask);

        // Should be back to original
        assert_eq!(data, original);
    }

    #[test]
    fn test_apply_mask_empty() {
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let mut data: Vec<u8> = vec![];
        apply_mask(&mut data, mask);
        assert!(data.is_empty());
    }

    #[test]
    fn test_apply_mask_short() {
        let mask = [0x37, 0xfa, 0x21, 0x3d];

        for len in 1..=15 {
            let original: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let mut data = original.clone();

            apply_mask(&mut data, mask);
            apply_mask(&mut data, mask);

            assert_eq!(data, original, "Failed for length {}", len);
        }
    }

    #[test]
    fn test_apply_mask_long() {
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let original: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let mut data = original.clone();

        apply_mask(&mut data, mask);
        assert_ne!(data, original);

        apply_mask(&mut data, mask);
        assert_eq!(data, original);
    }

    #[test]
    fn test_apply_mask_offset() {
        let mask = [0x01, 0x02, 0x03, 0x04];
        let mut data = vec![0x00; 8];

        apply_mask_offset(&mut data, mask, 1);

        // With offset 1, mask should be rotated: [0x02, 0x03, 0x04, 0x01]
        assert_eq!(data, vec![0x02, 0x03, 0x04, 0x01, 0x02, 0x03, 0x04, 0x01]);
    }

    #[test]
    fn test_generate_mask() {
        let mask1 = generate_mask();
        let mask2 = generate_mask();

        // Masks should be different (fastrand maintains thread-local state)
        assert_ne!(mask1, mask2);
    }
}
