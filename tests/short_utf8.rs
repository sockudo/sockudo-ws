#[test]
fn short_utf8_validation_matches_standard_library() {
    for length in 0..=64 {
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
