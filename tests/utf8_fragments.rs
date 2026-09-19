use sockudo_ws::utf8::validate_utf8_incomplete;
#[test]
fn complete_and_incomplete_fragments_preserve_utf8_boundaries() {
    for prefix in 0..64 {
        let mut bytes = vec![b'a'; prefix];
        bytes.extend_from_slice("世界🎉".as_bytes());
        assert_eq!(validate_utf8_incomplete(&bytes), (true, 0));
        bytes.pop();
        assert_eq!(validate_utf8_incomplete(&bytes), (true, 3));
        bytes.push(0xff);
        assert!(!validate_utf8_incomplete(&bytes).0);
    }
}
