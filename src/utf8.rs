//! SIMD-accelerated UTF-8 validation
//!
//! Uses `simdutf8` on its supported targets, including x86 and aarch64,
//! and its portable fallback elsewhere. Streaming validation retains an
//! incomplete code point between chunks instead of validating vectors alone.
//!
//! # References
//!
//! - [simdjson UTF-8 validation](https://github.com/simdjson/simdjson)
//! - [Validating UTF-8 In Less Than One Instruction Per Byte](https://arxiv.org/abs/2010.03090)
//! - [simdutf8](https://github.com/rusticstuff/simdutf8)

/// Validate that the input is valid UTF-8.
///
/// The dependency dispatches to its supported SIMD implementations and uses
/// the standard validator on other targets. Validation spans vector boundaries.
#[inline]
pub fn validate_utf8(data: &[u8]) -> bool {
    simdutf8::basic::from_utf8(data).is_ok()
}

// ============================================================================
// Incremental UTF-8 validation (chunked, SIMD per chunk)
// ============================================================================

/// Incremental UTF-8 validator for data that arrives in chunks.
///
/// Each chunk is validated with the SIMD validator except for an incomplete
/// trailing sequence (at most 3 bytes), which is carried over and checked once
/// the following chunk supplies the rest. Validating a message chunk by chunk
/// is therefore a single linear pass regardless of how it is fragmented, and an
/// invalid sequence is reported as soon as the chunk containing it is pushed.
#[derive(Debug, Clone, Default)]
pub struct Utf8Stream {
    carry: [u8; 4],
    carry_len: u8,
}

impl Utf8Stream {
    /// Create a validator with no pending bytes.
    #[inline]
    pub const fn new() -> Self {
        Self {
            carry: [0; 4],
            carry_len: 0,
        }
    }

    /// Forget any pending bytes and start validating a new message.
    #[inline]
    pub fn reset(&mut self) {
        self.carry_len = 0;
    }

    /// Resume from a message's unvalidated suffix before any validation error.
    ///
    /// After successful validation the carry is the entire pending state, so
    /// matching bytes already represent the result of resetting and re-pushing
    /// the suffix. Compare contents, not just length: raw processing may have
    /// started another message while leaving this validator unchanged.
    #[inline]
    pub(crate) fn resume(&mut self, suffix: &[u8]) -> bool {
        if suffix == &self.carry[..self.carry_len as usize] {
            return true;
        }
        self.reset();
        self.push(suffix)
    }

    /// Validate the next chunk of the message.
    ///
    /// Returns `false` as soon as an invalid sequence is complete enough to be
    /// rejected. A `true` result means everything so far is valid, possibly
    /// with an incomplete sequence still pending (see [`Utf8Stream::finish`]).
    #[inline]
    pub fn push(&mut self, mut chunk: &[u8]) -> bool {
        if self.carry_len > 0 {
            let width = utf8_sequence_width(self.carry[0]);
            let have = self.carry_len as usize;
            let take = (width - have).min(chunk.len());
            self.carry[have..have + take].copy_from_slice(&chunk[..take]);
            self.carry_len += take as u8;
            chunk = &chunk[take..];

            if (self.carry_len as usize) < width {
                // Still incomplete: reject as early as the prefix is hopeless.
                return incomplete_prefix_may_be_valid(&self.carry[..self.carry_len as usize]);
            }
            if std::str::from_utf8(&self.carry[..width]).is_err() {
                return false;
            }
            self.carry_len = 0;
        }

        let complete = complete_prefix_len(chunk);
        if !validate_utf8(&chunk[..complete]) {
            return false;
        }

        let tail = &chunk[complete..];
        self.carry[..tail.len()].copy_from_slice(tail);
        self.carry_len = tail.len() as u8;
        // Fail fast (RFC 6455 §8.1): an incomplete sequence that cannot become
        // valid is an error now, not once the rest of the bytes arrive.
        incomplete_prefix_may_be_valid(tail)
    }

    /// Returns `true` if no incomplete sequence is pending, i.e. the message
    /// validated so far is complete and valid UTF-8.
    #[inline]
    pub fn finish(&self) -> bool {
        self.carry_len == 0
    }

    /// Number of bytes held back as an incomplete trailing sequence.
    #[inline]
    pub fn pending(&self) -> usize {
        self.carry_len as usize
    }
}

/// Encoded length of the UTF-8 sequence introduced by `lead` (0 if `lead` is
/// not a valid leading byte).
#[inline]
fn utf8_sequence_width(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 0,
    }
}

/// Returns true if `prefix` (an incomplete multi-byte sequence: a lead byte and
/// zero or more continuation bytes) can still be completed into valid UTF-8.
///
/// Implements the second-byte constraints of RFC 3629 (no overlong forms, no
/// surrogates, nothing above U+10FFFF) so that e.g. `F4 90` or `ED A0` are
/// rejected without waiting for the remaining bytes.
#[inline]
fn incomplete_prefix_may_be_valid(prefix: &[u8]) -> bool {
    let Some(&lead) = prefix.first() else {
        return true;
    };
    let second_range: (u8, u8) = match lead {
        0xC2..=0xDF => (0x80, 0xBF),
        0xE0 => (0xA0, 0xBF),
        0xE1..=0xEC | 0xEE..=0xEF => (0x80, 0xBF),
        0xED => (0x80, 0x9F),
        0xF0 => (0x90, 0xBF),
        0xF1..=0xF3 => (0x80, 0xBF),
        0xF4 => (0x80, 0x8F),
        _ => return false,
    };
    if let Some(&second) = prefix.get(1)
        && !(second_range.0..=second_range.1).contains(&second)
    {
        return false;
    }
    prefix.iter().skip(2).all(|&b| (0x80..=0xBF).contains(&b))
}

/// Length of the prefix of `chunk` that does not end in the middle of a
/// multi-byte sequence. Only well-formed incomplete tails are held back;
/// anything else is left in the prefix for the validator to reject.
#[inline]
fn complete_prefix_len(chunk: &[u8]) -> usize {
    let n = chunk.len();
    for back in 1..=n.min(3) {
        let b = chunk[n - back];
        if b < 0x80 {
            return n;
        }
        if b >= 0xC0 {
            let width = utf8_sequence_width(b);
            return if width > back { n - back } else { n };
        }
    }
    n
}

// ============================================================================
// Streaming UTF-8 Validation (for fragmented messages)
// ============================================================================

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
            if !(0x10000..=0x10FFFF).contains(&cp) {
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

// ============================================================================
// Tests
// ============================================================================

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
    fn test_utf8_stream_chunked_matches_whole() {
        let text = "Hello, 世界! 🎉 κόσμε éà ".repeat(50);
        let bytes = text.as_bytes();
        for chunk in [1usize, 2, 3, 5, 7, 16, 64, 1000] {
            let mut v = Utf8Stream::new();
            for part in bytes.chunks(chunk) {
                assert!(v.push(part), "chunk size {chunk}");
            }
            assert!(v.finish(), "chunk size {chunk}");
        }
    }

    #[test]
    fn test_utf8_stream_rejects_invalid_split_anywhere() {
        // valid, then a complete invalid 4-byte sequence, then valid
        let mut data = "κόσμε".as_bytes().to_vec();
        data.extend_from_slice(&[0xF4, 0x90, 0x80, 0x80]);
        data.extend_from_slice(b"edited");
        for chunk in 1..=data.len() {
            let mut v = Utf8Stream::new();
            let mut rejected = false;
            for part in data.chunks(chunk) {
                if !v.push(part) {
                    rejected = true;
                    break;
                }
            }
            assert!(rejected, "chunk size {chunk}");
        }
        // Rejection must happen as soon as the invalid sequence is complete.
        let mut v = Utf8Stream::new();
        assert!(v.push("κόσμε".as_bytes()));
        assert!(!v.push(&[0xF4, 0x90, 0x80, 0x80]));
    }

    #[test]
    fn test_utf8_stream_incomplete_tail() {
        let mut v = Utf8Stream::new();
        assert!(v.push(&[0xE4, 0xB8]));
        assert_eq!(v.pending(), 2);
        assert!(!v.finish());
        assert!(v.push(&[0xAD]));
        assert!(v.finish());

        // hopeless prefixes are rejected before the sequence completes
        let mut v = Utf8Stream::new();
        assert!(!v.push(&[0xED, 0xA0])); // surrogate
        let mut v = Utf8Stream::new();
        assert!(!v.push(&[0xF4, 0x90])); // > U+10FFFF
        let mut v = Utf8Stream::new();
        assert!(v.push(&[0xF4]));
        assert!(!v.push(&[0x90]));
        let mut v = Utf8Stream::new();
        assert!(!v.push(&[0xE0, 0x80])); // overlong
        let mut v = Utf8Stream::new();
        assert!(!v.push(&[0xC0])); // overlong lead
        let mut v = Utf8Stream::new();
        assert!(!v.push(&[0xE4, 0x41])); // non-continuation
        // valid prefixes are accepted and completed
        let mut v = Utf8Stream::new();
        assert!(v.push(&[0xF4, 0x8F]));
        assert!(v.push(&[0xBF, 0xBF]));
        assert!(v.finish());

        // invalid lead byte is rejected immediately, not carried
        let mut v = Utf8Stream::new();
        assert!(!v.push(&[0xFF]));
        let mut v = Utf8Stream::new();
        assert!(!v.push(b"ab\x80"));
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

    #[test]
    fn test_boundary_lengths() {
        // Test various lengths around SIMD boundaries (16, 32, 64 bytes)
        for len in [15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129] {
            let ascii = "a".repeat(len);
            assert!(
                validate_utf8(ascii.as_bytes()),
                "Failed for ASCII length {}",
                len
            );

            // Mixed content
            if len >= 9 {
                let prefix_len = (len - 9) / 2;
                let suffix_len = len - 9 - prefix_len;
                let mixed = format!("{}日本語{}", "a".repeat(prefix_len), "b".repeat(suffix_len));
                assert!(
                    validate_utf8(mixed.as_bytes()),
                    "Failed for mixed length {}",
                    len
                );
            }
        }
    }

    #[test]
    fn short_utf8_matches_std_at_every_byte_position() {
        for len in [1, 7, 8, 15, 16, 31, 32, 34, 63, 64, 65] {
            for sequence in [
                &[0x80][..],
                &[0xFF],
                "é".as_bytes(),
                "世".as_bytes(),
                "🎉".as_bytes(),
            ] {
                for start in 0..len {
                    let mut data = vec![b'a'; len];
                    let copied = sequence.len().min(len - start);
                    data[start..start + copied].copy_from_slice(&sequence[..copied]);
                    assert_eq!(validate_utf8(&data), std::str::from_utf8(&data).is_ok());
                }
            }
        }
    }

    #[test]
    fn valid_utf8_crossing_simd_boundaries_is_accepted() {
        for boundary in [16, 32, 64] {
            for sequence in ["é".as_bytes(), "世".as_bytes(), "🎉".as_bytes()] {
                for bytes_before_boundary in 1..sequence.len() {
                    let start = boundary - bytes_before_boundary;
                    let mut data = vec![b'a'; 128];
                    data[start..start + sequence.len()].copy_from_slice(sequence);

                    assert!(validate_utf8(&data));
                }
            }
        }
    }
}
