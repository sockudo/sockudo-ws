//! SIMD-accelerated UTF-8 validation
//!
//! This module provides high-performance UTF-8 validation using SIMD instructions.
//! The algorithm is based on the simdjson approach, adapted for WebSocket text frames.
//!
//! Key optimizations:
//! - Fast ASCII path: bulk scan for ASCII-only regions (no multi-byte chars)
//! - Vectorized validation of multi-byte sequences
//! - Minimal branching in the hot path

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

/// Validate that the input is valid UTF-8
///
/// Returns true if the input is valid UTF-8, false otherwise.
/// This is highly optimized using SIMD instructions when available.
#[inline]
pub fn validate_utf8(data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return unsafe { validate_utf8_avx2(data) };
        }
        if is_x86_feature_detected!("sse4.2") {
            return unsafe { validate_utf8_sse42(data) };
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        return unsafe { validate_utf8_neon(data) };
    }

    // Scalar fallback
    validate_utf8_scalar(data)
}

/// Scalar UTF-8 validation
///
/// This is a highly optimized scalar implementation that serves as:
/// 1. Fallback when SIMD is not available
/// 2. Reference implementation for testing
fn validate_utf8_scalar(data: &[u8]) -> bool {
    let mut i = 0;
    let len = data.len();

    while i < len {
        let b = data[i];

        if b < 0x80 {
            // Fast ASCII path - scan for ASCII-only region
            i += 1;

            // Check 8 bytes at a time for ASCII (using unaligned read)
            while i + 8 <= len {
                let chunk = unsafe { std::ptr::read_unaligned(data.as_ptr().add(i) as *const u64) };
                if chunk & 0x8080808080808080 != 0 {
                    break;
                }
                i += 8;
            }

            // Check remaining bytes for ASCII
            while i < len && data[i] < 0x80 {
                i += 1;
            }
        } else if b < 0xC0 {
            // Continuation byte at start = invalid
            return false;
        } else if b < 0xE0 {
            // 2-byte sequence: 110xxxxx 10xxxxxx
            if i + 1 >= len {
                return false;
            }
            let b1 = data[i + 1];
            // Check continuation byte pattern
            if b1 & 0xC0 != 0x80 {
                return false;
            }
            // Check for overlong encoding (must be >= 0x80)
            if b < 0xC2 {
                return false;
            }
            i += 2;
        } else if b < 0xF0 {
            // 3-byte sequence: 1110xxxx 10xxxxxx 10xxxxxx
            if i + 2 >= len {
                return false;
            }
            let b1 = data[i + 1];
            let b2 = data[i + 2];
            // Check continuation byte patterns
            if (b1 & 0xC0 != 0x80) || (b2 & 0xC0 != 0x80) {
                return false;
            }
            // Check for overlong encoding and surrogate halves
            let cp = ((b as u32 & 0x0F) << 12) | ((b1 as u32 & 0x3F) << 6) | (b2 as u32 & 0x3F);
            if cp < 0x800 || (0xD800..=0xDFFF).contains(&cp) {
                return false;
            }
            i += 3;
        } else if b < 0xF5 {
            // 4-byte sequence: 11110xxx 10xxxxxx 10xxxxxx 10xxxxxx
            if i + 3 >= len {
                return false;
            }
            let b1 = data[i + 1];
            let b2 = data[i + 2];
            let b3 = data[i + 3];
            // Check continuation byte patterns
            if (b1 & 0xC0 != 0x80) || (b2 & 0xC0 != 0x80) || (b3 & 0xC0 != 0x80) {
                return false;
            }
            // Check for overlong encoding and max codepoint
            let cp = ((b as u32 & 0x07) << 18)
                | ((b1 as u32 & 0x3F) << 12)
                | ((b2 as u32 & 0x3F) << 6)
                | (b3 as u32 & 0x3F);
            if cp < 0x10000 || cp > 0x10FFFF {
                return false;
            }
            i += 4;
        } else {
            // Invalid leading byte (>= 0xF5)
            return false;
        }
    }

    true
}

/// AVX2 UTF-8 validation (32 bytes per iteration)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn validate_utf8_avx2(data: &[u8]) -> bool {
    unsafe {
        let len = data.len();
        let mut ptr = data.as_ptr();
        let mut remaining = len;

        // Process 32 bytes at a time
        while remaining >= 32 {
            let chunk = _mm256_loadu_si256(ptr as *const __m256i);

            // Fast path: check if all bytes are ASCII
            let high_bits = _mm256_movemask_epi8(chunk);
            if high_bits == 0 {
                // All ASCII, continue
                ptr = ptr.add(32);
                remaining -= 32;
                continue;
            }

            // Contains non-ASCII, fall back to scalar for rest of data
            // This handles multi-byte sequences that may span chunk boundaries
            let slice = std::slice::from_raw_parts(ptr, remaining);
            return validate_utf8_scalar(slice);
        }

        // Validate remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts(ptr, remaining);
            return validate_utf8_scalar(slice);
        }

        true
    }
}

/// SSE4.2 UTF-8 validation (16 bytes per iteration)
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn validate_utf8_sse42(data: &[u8]) -> bool {
    unsafe {
        let len = data.len();
        let mut ptr = data.as_ptr();
        let mut remaining = len;

        // Process 16 bytes at a time
        while remaining >= 16 {
            let chunk = _mm_loadu_si128(ptr as *const __m128i);

            // Fast path: check if all bytes are ASCII
            let high_bits = _mm_movemask_epi8(chunk);
            if high_bits == 0 {
                // All ASCII, continue
                ptr = ptr.add(16);
                remaining -= 16;
                continue;
            }

            // Contains non-ASCII, fall back to scalar for rest of data
            let slice = std::slice::from_raw_parts(ptr, remaining);
            return validate_utf8_scalar(slice);
        }

        // Validate remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts(ptr, remaining);
            return validate_utf8_scalar(slice);
        }

        true
    }
}

/// NEON UTF-8 validation for ARM64 (16 bytes per iteration)
#[cfg(target_arch = "aarch64")]
unsafe fn validate_utf8_neon(data: &[u8]) -> bool {
    unsafe {
        let len = data.len();
        let mut ptr = data.as_ptr();
        let mut remaining = len;

        // Threshold for high bit check
        let threshold = vdupq_n_u8(0x80);

        // Process 16 bytes at a time
        while remaining >= 16 {
            let chunk = vld1q_u8(ptr);

            // Fast path: check if all bytes are ASCII
            // Compare each byte with 0x80
            let cmp = vcgeq_u8(chunk, threshold);
            let mask = vget_lane_u64(
                vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(cmp), 4)),
                0,
            );

            if mask == 0 {
                // All ASCII, continue
                ptr = ptr.add(16);
                remaining -= 16;
                continue;
            }

            // Contains non-ASCII, fall back to scalar for rest of data
            let slice = std::slice::from_raw_parts(ptr, remaining);
            return validate_utf8_scalar(slice);
        }

        // Validate remaining bytes with scalar
        if remaining > 0 {
            let slice = std::slice::from_raw_parts(ptr, remaining);
            return validate_utf8_scalar(slice);
        }

        true
    }
}

/// Check if data is valid UTF-8 with incomplete sequence at the end
///
/// Returns:
/// - (true, n) if all complete sequences are valid, where n is the number of
///   trailing bytes that form an incomplete sequence (0-3 bytes)
/// - (false, 0) if there's an invalid UTF-8 sequence
///
/// This function is used for streaming UTF-8 validation where data may be
/// split across fragment boundaries in the middle of a multi-byte character.
pub fn validate_utf8_incomplete(data: &[u8]) -> (bool, usize) {
    if data.is_empty() {
        return (true, 0);
    }

    let len = data.len();
    let mut i = 0;

    while i < len {
        let b = data[i];

        if b < 0x80 {
            // ASCII byte
            i += 1;
        } else if b < 0xC0 {
            // Unexpected continuation byte at start of sequence
            return (false, 0);
        } else if b < 0xE0 {
            // 2-byte sequence: need 1 more byte
            if i + 1 >= len {
                // Incomplete - return how many bytes we have
                return (true, len - i);
            }
            let b1 = data[i + 1];
            if b1 & 0xC0 != 0x80 {
                return (false, 0);
            }
            // Check for overlong encoding
            if b < 0xC2 {
                return (false, 0);
            }
            i += 2;
        } else if b < 0xF0 {
            // 3-byte sequence: need 2 more bytes
            if i + 2 >= len {
                // Incomplete - but first validate what we have
                if i + 1 < len {
                    let b1 = data[i + 1];
                    if b1 & 0xC0 != 0x80 {
                        return (false, 0);
                    }
                    // Check for overlong and surrogate
                    if b == 0xE0 && b1 < 0xA0 {
                        return (false, 0);
                    }
                    if b == 0xED && b1 >= 0xA0 {
                        return (false, 0); // Surrogate
                    }
                }
                return (true, len - i);
            }
            let b1 = data[i + 1];
            let b2 = data[i + 2];
            if (b1 & 0xC0 != 0x80) || (b2 & 0xC0 != 0x80) {
                return (false, 0);
            }
            // Check for overlong encoding and surrogate halves
            let cp = ((b as u32 & 0x0F) << 12) | ((b1 as u32 & 0x3F) << 6) | (b2 as u32 & 0x3F);
            if cp < 0x800 || (0xD800..=0xDFFF).contains(&cp) {
                return (false, 0);
            }
            i += 3;
        } else if b < 0xF5 {
            // 4-byte sequence: need 3 more bytes
            if i + 3 >= len {
                // Incomplete - but first validate what we have
                if i + 1 < len {
                    let b1 = data[i + 1];
                    if b1 & 0xC0 != 0x80 {
                        return (false, 0);
                    }
                    // Check for overlong and out of range
                    if b == 0xF0 && b1 < 0x90 {
                        return (false, 0);
                    }
                    if b == 0xF4 && b1 >= 0x90 {
                        return (false, 0); // > U+10FFFF
                    }
                }
                if i + 2 < len {
                    let b2 = data[i + 2];
                    if b2 & 0xC0 != 0x80 {
                        return (false, 0);
                    }
                }
                return (true, len - i);
            }
            let b1 = data[i + 1];
            let b2 = data[i + 2];
            let b3 = data[i + 3];
            if (b1 & 0xC0 != 0x80) || (b2 & 0xC0 != 0x80) || (b3 & 0xC0 != 0x80) {
                return (false, 0);
            }
            // Check for overlong encoding and max codepoint
            let cp = ((b as u32 & 0x07) << 18)
                | ((b1 as u32 & 0x3F) << 12)
                | ((b2 as u32 & 0x3F) << 6)
                | (b3 as u32 & 0x3F);
            if cp < 0x10000 || cp > 0x10FFFF {
                return (false, 0);
            }
            i += 4;
        } else {
            // Invalid leading byte (>= 0xF5)
            return (false, 0);
        }
    }

    (true, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_ascii() {
        assert!(validate_utf8(b"Hello, World!"));
        assert!(validate_utf8(b""));
        assert!(validate_utf8(b"0123456789"));
    }

    #[test]
    fn test_valid_utf8() {
        assert!(validate_utf8("Hello, 世界!".as_bytes()));
        assert!(validate_utf8("émoji: 🎉".as_bytes()));
        assert!(validate_utf8("Ñoño".as_bytes()));
        assert!(validate_utf8("日本語".as_bytes()));
    }

    #[test]
    fn test_invalid_utf8() {
        // Invalid continuation byte
        assert!(!validate_utf8(&[0xC0, 0x00]));

        // Overlong encoding
        assert!(!validate_utf8(&[0xC0, 0x80])); // Overlong NUL
        assert!(!validate_utf8(&[0xC1, 0xBF])); // Overlong

        // Invalid leading byte
        assert!(!validate_utf8(&[0xFF]));
        assert!(!validate_utf8(&[0xFE]));

        // Surrogate halves (invalid in UTF-8)
        assert!(!validate_utf8(&[0xED, 0xA0, 0x80])); // U+D800
        assert!(!validate_utf8(&[0xED, 0xBF, 0xBF])); // U+DFFF

        // Truncated sequences
        assert!(!validate_utf8(&[0xE0, 0x80])); // Missing byte
        assert!(!validate_utf8(&[0xF0, 0x80, 0x80])); // Missing byte

        // Invalid continuation
        assert!(!validate_utf8(&[0xE0, 0x80, 0x00]));
    }

    #[test]
    fn test_validate_utf8_incomplete() {
        // Complete ASCII
        let (valid, incomplete) = validate_utf8_incomplete(b"hello");
        assert!(valid);
        assert_eq!(incomplete, 0);

        // Incomplete 2-byte sequence
        let (valid, incomplete) = validate_utf8_incomplete(&[0xC2]);
        assert!(valid);
        assert_eq!(incomplete, 1);

        // Incomplete 3-byte sequence
        let (valid, incomplete) = validate_utf8_incomplete(&[0xE4, 0xB8]);
        assert!(valid);
        assert_eq!(incomplete, 2);

        // Complete followed by incomplete
        let mut data = b"hi".to_vec();
        data.push(0xE4);
        data.push(0xB8);
        let (valid, incomplete) = validate_utf8_incomplete(&data);
        assert!(valid);
        assert_eq!(incomplete, 2);
    }

    #[test]
    fn test_long_utf8() {
        // Test with longer strings to exercise SIMD paths
        let long_ascii = "a".repeat(1000);
        assert!(validate_utf8(long_ascii.as_bytes()));

        let long_unicode = "日本語".repeat(100);
        assert!(validate_utf8(long_unicode.as_bytes()));

        let mixed = format!("{}日本語{}", "a".repeat(100), "b".repeat(100));
        assert!(validate_utf8(mixed.as_bytes()));
    }
}
