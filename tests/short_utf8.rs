#[test]
fn short_utf8_validation_matches_standard_library() {
    for length in 0..=65 {
        let mut bytes = vec![b'a'; length];
        assert!(sockudo_ws::utf8::validate_utf8(&bytes));
        for offset in 0..length {
            bytes[offset] = 0xff;
            assert_eq!(
                sockudo_ws::utf8::validate_utf8(&bytes),
                std::str::from_utf8(&bytes).is_ok()
            );
            bytes[offset] = b'a';
        }
    }
}

#[test]
fn short_utf8_boundaries_preserve_non_ascii_validation() {
    for length in [1usize, 8, 16, 31, 32, 63, 64, 65] {
        for suffix in [
            "é".as_bytes(),
            "界".as_bytes(),
            "🦀".as_bytes(),
            b"\xc0\x80",
            b"\xed\xa0\x80",
            b"\xf4\x90\x80\x80",
            b"\xe2\x82",
            b"\x80",
        ] {
            let mut bytes = vec![b'a'; length.saturating_sub(suffix.len())];
            bytes.extend_from_slice(suffix);
            assert_eq!(
                sockudo_ws::utf8::validate_utf8(&bytes),
                std::str::from_utf8(&bytes).is_ok(),
                "length {length}, suffix {suffix:?}"
            );
        }
    }
}

#[test]
fn short_utf8_prefixes_preserve_validation_at_every_byte_offset() {
    for length in 1usize..=65 {
        for alignment in 0..8 {
            for sequence in [
                "é".as_bytes(),
                "界".as_bytes(),
                "🦀".as_bytes(),
                b"\xc0\x80",
                b"\xed\xa0\x80",
                b"\xf4\x90\x80\x80",
                b"\xe2\x82",
                b"\x80",
            ] {
                if sequence.len() > length {
                    continue;
                }
                for offset in 0..=length - sequence.len() {
                    let mut storage = vec![b'a'; alignment + length];
                    let bytes = &mut storage[alignment..];
                    bytes[offset..offset + sequence.len()].copy_from_slice(sequence);
                    assert_eq!(
                        sockudo_ws::utf8::validate_utf8(bytes),
                        std::str::from_utf8(bytes).is_ok(),
                        "length {length}, alignment {alignment}, offset {offset}, sequence {sequence:?}"
                    );
                }
            }
        }
    }
}
