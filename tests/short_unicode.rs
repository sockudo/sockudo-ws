use sockudo_ws::utf8::validate_utf8;

#[test]
fn short_multibyte_text_is_valid_at_ascii_fast_path_boundaries() {
    for size in [15, 16, 31, 32, 63, 64] {
        for character in ["é", "中", "😀"] {
            let text = format!("{}{}", "a".repeat(size - character.len()), character);
            assert!(validate_utf8(text.as_bytes()));
            assert!(!validate_utf8(&text.as_bytes()[..text.len() - 1]));
        }
    }
}
