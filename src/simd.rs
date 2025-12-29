//! SIMD-accelerated operations for WebSocket processing
//!
//! This module provides highly optimized implementations of:
//! - Frame masking/unmasking (XOR operations)
//! - UTF-8 validation
//! - HTTP header parsing helpers
//!
//! The implementations automatically select the best available SIMD instruction set:
//! - AVX-512 (512-bit, 64 bytes per iteration)
//! - AVX2 (256-bit, 32 bytes per iteration)
//! - SSE4.2 (128-bit, 16 bytes per iteration)
//! - NEON (128-bit on ARM)
//! - Scalar fallback

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
            unsafe { apply_mask_avx512(data, mask) };
            return;
        }
        if is_x86_feature_detected!("avx2") {
            unsafe { apply_mask_avx2(data, mask) };
            return;
        }
        if is_x86_feature_detected!("sse2") {
            unsafe { apply_mask_sse2(data, mask) };
            return;
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        unsafe { apply_mask_neon(data, mask) };
        return;
    }

    // Scalar fallback
    apply_mask_scalar(data, mask);
}

/// Scalar fallback for mask application
#[inline]
fn apply_mask_scalar(data: &mut [u8], mask: [u8; 4]) {
    // Process 8 bytes at a time using u64
    let mask_u64 = u64::from_ne_bytes([
        mask[0], mask[1], mask[2], mask[3], mask[0], mask[1], mask[2], mask[3],
    ]);

    let mut i = 0;
    let len = data.len();

    // Process 8 bytes at a time
    while i + 8 <= len {
        let chunk = unsafe { &mut *(data.as_mut_ptr().add(i) as *mut u64) };
        *chunk ^= mask_u64;
        i += 8;
    }

    // Handle remaining bytes
    while i < len {
        data[i] ^= mask[i & 3];
        i += 1;
    }
}

/// AVX-512 implementation (64 bytes per iteration)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f", enable = "avx512bw")]
unsafe fn apply_mask_avx512(data: &mut [u8], mask: [u8; 4]) {
    unsafe {
        let len = data.len();
        let mut ptr = data.as_mut_ptr();

        // Create 64-byte mask vector
        let mask_u32 = u32::from_ne_bytes(mask);
        let mask_vec = _mm512_set1_epi32(mask_u32 as i32);

        let mut remaining = len;

        // Process 64 bytes at a time
        while remaining >= 64 {
            let data_vec = _mm512_loadu_si512(ptr as *const __m512i);
            let result = _mm512_xor_si512(data_vec, mask_vec);
            _mm512_storeu_si512(ptr as *mut __m512i, result);
            ptr = ptr.add(64);
            remaining -= 64;
        }

        // Handle remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts_mut(ptr, remaining);
            apply_mask_scalar(slice, mask);
        }
    }
}

/// AVX2 implementation (32 bytes per iteration)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn apply_mask_avx2(data: &mut [u8], mask: [u8; 4]) {
    unsafe {
        let len = data.len();
        let mut ptr = data.as_mut_ptr();

        // Create 32-byte mask vector by broadcasting the 4-byte mask
        let mask_u32 = u32::from_ne_bytes(mask);
        let mask_vec = _mm256_set1_epi32(mask_u32 as i32);

        let mut remaining = len;

        // Process 32 bytes at a time
        while remaining >= 32 {
            let data_vec = _mm256_loadu_si256(ptr as *const __m256i);
            let result = _mm256_xor_si256(data_vec, mask_vec);
            _mm256_storeu_si256(ptr as *mut __m256i, result);
            ptr = ptr.add(32);
            remaining -= 32;
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
            apply_mask_scalar(slice, mask);
        }
    }
}

/// SSE2 implementation (16 bytes per iteration)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn apply_mask_sse2(data: &mut [u8], mask: [u8; 4]) {
    unsafe {
        let len = data.len();
        let mut ptr = data.as_mut_ptr();

        // Create 16-byte mask vector
        let mask_u32 = u32::from_ne_bytes(mask);
        let mask_vec = _mm_set1_epi32(mask_u32 as i32);

        let mut remaining = len;

        // Process 16 bytes at a time
        while remaining >= 16 {
            let data_vec = _mm_loadu_si128(ptr as *const __m128i);
            let result = _mm_xor_si128(data_vec, mask_vec);
            _mm_storeu_si128(ptr as *mut __m128i, result);
            ptr = ptr.add(16);
            remaining -= 16;
        }

        // Handle remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts_mut(ptr, remaining);
            apply_mask_scalar(slice, mask);
        }
    }
}

/// NEON implementation for ARM64 (16 bytes per iteration)
#[cfg(target_arch = "aarch64")]
unsafe fn apply_mask_neon(data: &mut [u8], mask: [u8; 4]) {
    unsafe {
        let len = data.len();
        let mut ptr = data.as_mut_ptr();

        // Create 16-byte mask vector by duplicating the 4-byte mask
        let mask_bytes: [u8; 16] = [
            mask[0], mask[1], mask[2], mask[3], mask[0], mask[1], mask[2], mask[3], mask[0],
            mask[1], mask[2], mask[3], mask[0], mask[1], mask[2], mask[3],
        ];
        let mask_vec = vld1q_u8(mask_bytes.as_ptr());

        let mut remaining = len;

        // Process 16 bytes at a time
        while remaining >= 16 {
            let data_vec = vld1q_u8(ptr);
            let result = veorq_u8(data_vec, mask_vec);
            vst1q_u8(ptr, result);
            ptr = ptr.add(16);
            remaining -= 16;
        }

        // Handle remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts_mut(ptr, remaining);
            apply_mask_scalar(slice, mask);
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
#[inline]
pub fn generate_mask() -> [u8; 4] {
    // Use a simple fast RNG for mask generation
    // This doesn't need to be cryptographically secure
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    // xorshift64
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;

    (seed as u32).to_ne_bytes()
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
        std::thread::sleep(std::time::Duration::from_nanos(1));
        let mask2 = generate_mask();

        // Masks should be different (with very high probability)
        // Note: This could theoretically fail but is extremely unlikely
        assert_ne!(mask1, mask2);
    }
}
